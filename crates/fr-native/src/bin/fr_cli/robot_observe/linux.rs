//! Native observation uses the canonical supervisor, consent and cleanup path.
use super::{Failure, Minimum, Request, report};
use crate::session_control::{self, Completion, Owner, observation::Publisher};
use asupersync::{
    cx::Cx,
    runtime::RuntimeBuilder,
    signal::{Signal, SignalKind, signal},
    types::{Budget, CancelKind},
};
use fr_client::robot::live::{Evidence, Phase};
use fr_native::desktop::{
    Configuration as DesktopConfiguration, Desktop,
    reconnect::{Mode, Observation, Session, Ui},
};
use fr_wire::display::Catalog;
use frd::{
    media::PresentationStage,
    native_connection::{
        AddressFamily, Client, Configuration, LocalApi, PeerSelector, TailnetError,
        reconnect::{self, CallbackError, Status},
    },
    session_startup::{ObserverPolicy, Presentation},
};
use std::{
    cell::Cell,
    future::{Future, poll_fn},
    path::PathBuf,
    pin::pin,
    rc::Rc,
    task::Poll,
    time::{Duration, Instant},
};

fn failure(code: &'static str, next: &'static str) -> Failure { Failure::new(code, next, 1) }

#[allow(clippy::too_many_lines)] // One original attempt, mandatory cleanup, then reporting.
pub(super) fn run(request: &Request) -> Result<String, Failure> {
    let mut shutdown = Shutdown::new()?;
    let runtime = RuntimeBuilder::current_thread().enable_platform_reactor(true).build()
        .map_err(|_| failure("runtime_unavailable", "Check local runtime resources."))?;
    let cx = runtime.handle().try_request_cx_with_budget(Budget::INFINITE)
        .map_err(|_| failure("runtime_unavailable", "Check local runtime resources."))?;
    let connection = &request.connection;
    let roots = fr_tailnet::trust::read_certificates(&connection.target.roots).map_err(|_| invalid_roots())?;
    if roots.len() > 256 { return Err(invalid_roots()); }
    let api = match &request.socket {
        Some(path) => LocalApi::new(path).map_err(tailnet)?,
        None => LocalApi::installed(),
    };
    let mut client = Client::new(api, roots, Duration::from_secs(5)).map_err(|_| failure(
        "invalid_native_configuration", "Check the local trust store and installed-tailnet native configuration."))?;
    let display = connection.x_display.clone().or_else(|| std::env::var("DISPLAY").ok()).ok_or_else(|| failure(
        "display_unavailable", "Set DISPLAY or --x-display for an existing local X11 session; observation is not headless."))?;
    let xauthority = std::env::var_os("XAUTHORITY").map(PathBuf::from);
    let mut epoch = [0; 16];
    cx.random_bytes(&mut epoch);
    let mut desktop = DesktopConfiguration::new(&connection.worker, &display, xauthority.as_deref(), u128::from_be_bytes(epoch))
        .map_err(|_| failure("invalid_worker_configuration", "Select a trusted locally installed worker and matching local graphical-session settings."))?;
    if let Some((width, height)) = connection.fit_window {
        desktop = desktop.with_fitted_window(width, height).map_err(|_| failure("invalid_window_size", "Choose bounded even dimensions for --fit."))?;
    }
    if request.screenshot.is_some() {
        desktop = desktop.with_initial_snapshot();
    }
    // The optional endpoint is the SAME one used by fr connect/status/close.
    // Bind before dialing and retain it through confirmed native cleanup.
    let local = runtime.handle().try_request_cx_with_budget(Budget::INFINITE)
        .map_err(|_| failure("runtime_unavailable", "Check local runtime resources."))?;
    let publisher = request.live.then(Publisher::default);
    let mut owner = match &publisher {
        Some(publisher) => Some(runtime.block_on(Owner::bind(&local, &connection.target.node))
            .map_err(owner_failure)?.with_observations(publisher.clone())),
        None => None,
    };
    let info = owner.as_ref().map(Owner::info);
    let receipt_time = Rc::new(Cell::new(None));
    let missing_display = Rc::new(Cell::new(false));
    let insufficient = Rc::new(Cell::new(false));
    let interface = Interface {
        display: connection.display, minimum: request.minimum,
        receipt_time: receipt_time.clone(), missing_display: missing_display.clone(),
        insufficient: insufficient.clone(), publisher,
    };
    let mut application = if request.live {
        Session::new(desktop, ObserverPolicy::default(), Mode::Observe, interface)
    } else {
        Session::observing_once(desktop, ObserverPolicy::default(), interface)
    };
    let selector = if connection.target.by_name { PeerSelector::Name(&connection.target.node) } else { PeerSelector::StableId(&connection.target.node) };
    let operation = client.run_observing(
        cx.clone(), runtime.handle(), selector,
        Configuration {
            port: connection.target.port,
            family: if connection.target.ipv6 { AddressFamily::Ipv6 } else { AddressFamily::Automatic },
            ..Default::default()
        },
        fr_client::native::observation_offer(),
        reconnect::Policy { max_attempts: 1, ..Default::default() },
        &mut application,
    );
    let stopped = Cell::new(false);
    let result = match owner.as_mut() {
        Some(owner) => runtime.block_on(drain_on_shutdown(
            &cx, &stopped, operation, local_or_signal(owner.wait_for_stop(&local), shutdown.requested()),
        )),
        None => runtime.block_on(drain_on_shutdown(&cx, &stopped, operation, shutdown.requested())),
    };
    let clean = application.desktop().is_none() && application.cleanup_failure().is_none()
        && !matches!(result, Err(reconnect::Failure::Cleanup | reconnect::Failure::CleanupExpired));
    if let Some(owner) = owner.as_mut() {
        // Reply failure changes neither native cleanup nor the original result.
        // The requesting close command reports a missing reply as unknown.
        let _ = runtime.block_on(owner.complete(&local, if clean { Completion::Confirmed } else { Completion::Unconfirmed }));
    }
    let result = completion_gate(result, clean, stopped.get(), missing_display.get());
    let result = if clean && !stopped.get() && insufficient.get() {
        Err(failure("observation_evidence_unavailable", "The initial native receipt did not meet the requested evidence level."))
    } else { result };
    let result = result.and_then(|()| {
            if let Some(info) = info {
                return closed(info, request.json, application.disconnect_outcome());
            }
            report::render(
                request, application.initial_observation(), receipt_time.get(),
                application.disconnect_outcome(), application.take_initial_snapshot(),
            )
        });
    crate::closure::preserve(result, application.disconnect_outcome())
}

fn closed(info: session_control::Info, json: bool, close: Option<frd::session_startup::ViewerCloseOutcome>) -> Result<String, Failure> {
    if json {
        let exchange = close.map(|outcome| serde_json::from_str::<serde_json::Value>(&crate::closure::Report::from(outcome).json()))
            .transpose().map_err(|_| failure("serialization_error", "Could not render the original closing exchange."))?;
        serde_json::to_string(&serde_json::json!({
            "schema_version": 1, "timestamp_unix_ms": report::unix_ms(), "outcome": "success",
            "data": { "scope": "local_connection", "connection_id": info.instance, "target": info.target,
                "closed": true, "local_cleanup_confirmed": true, "close_exchange": exchange }
        })).map_err(|_| failure("serialization_error", "Could not render session closure."))
    } else {
        let exchange = close.map_or_else(|| "No completed host close exchange; remote cleanup is unknown.".to_owned(), |outcome| crate::closure::Report::from(outcome).text());
        Ok(format!("View-only observation owner stopped; local native cleanup confirmed.\nTarget: {}\nConnection ID: {}\n{exchange}\n", info.target, info.instance))
    }
}
fn owner_failure(error: session_control::Error) -> Failure {
    let code = match error {
        session_control::Error::AlreadyRunning => "local_session_already_running",
        session_control::Error::InvalidTarget => "invalid_local_target",
        session_control::Error::UntrustedPeer => "local_session_untrusted",
        _ => "local_session_unavailable",
    };
    failure(code, "Check the exact local target and its existing foreground owner before opening another session; no remote connection was opened.")
}
fn invalid_roots() -> Failure {
    failure("invalid_trust_store", "Use a locally provisioned regular non-symlink PEM root file, at most 1 MiB and 256 certificates. Never use roots supplied by the contacted peer.")
}
fn completion_gate(result: Result<(), reconnect::Failure>, native_clean: bool, stopped: bool, missing_display: bool) -> Result<(), Failure> {
    if !native_clean || matches!(result, Err(reconnect::Failure::Cleanup | reconnect::Failure::CleanupExpired)) {
        return Err(failure("native_cleanup_incomplete", "The observation ended without confirmed native cleanup. Inspect remaining workers before another connection."));
    }
    if stopped {
        return Err(Failure::new("cancelled", "Observation was stopped and its supervisor completed local cleanup; no actions were replayed.", 130));
    }
    if missing_display {
        return Err(failure("display_not_offered", "The requested display is not in the approved catalog; --display only requires exactly one display. Inspect fr displays before another observation."));
    }
    result.map_err(|error| match error {
        reconnect::Failure::Cancelled => Failure::new("cancelled", "Observation cancelled.", 130),
        reconnect::Failure::Connection(frd::native_connection::Error::Tailnet(error)) => tailnet(error),
        _ => failure("native_observation_failed", "Native observation or its closing exchange failed. Check local display/worker permissions and host approval; no retry or screenshot was performed."),
    })
}
fn tailnet(error: TailnetError) -> Failure {
    let code = match error {
        TailnetError::Cancelled => return Failure::new("cancelled", "Observation cancelled.", 130),
        TailnetError::LocalApiUnavailable => "tailscale_unavailable",
        TailnetError::BackendNotRunning => "tailscale_not_running",
        TailnetError::UntrustedLocalApi => "untrusted_localapi",
        TailnetError::Timeout => "tailnet_lookup_timeout",
        TailnetError::HandshakeTimeout => "host_not_answering",
        TailnetError::KeyExpired => "tailnet_key_expired",
        TailnetError::PeerCertificateRejected => "tls_certificate_rejected",
        TailnetError::NativeRouteIo => "tailnet_route_unavailable",
        _ => "tailnet_identity_refused",
    };
    failure(code, "Check installed Tailscale, the exact discovered host, host service and local trusted roots. Never disable identity or certificate checks.")
}
fn evidence(stage: PresentationStage) -> Evidence {
    match stage {
        PresentationStage::DecodedOnly => Evidence::Decoded,
        PresentationStage::SubmittedToCompositor => Evidence::SubmittedToCompositor,
    }
}
struct Interface {
    display: crate::options::DisplayChoice,
    minimum: Minimum,
    receipt_time: Rc<Cell<Option<u64>>>,
    missing_display: Rc<Cell<bool>>,
    insufficient: Rc<Cell<bool>>,
    publisher: Option<Publisher>,
}
impl Ui for Interface {
    fn choose(&mut self, _: u8, catalog: &Catalog) -> Result<Option<u128>, CallbackError> {
        let selected = self.display.select(catalog);
        if selected.is_none() { self.missing_display.set(true); return Err(CallbackError); }
        Ok(selected)
    }
    fn approval(&mut self, _: u8, _: fr_client::startup::ApprovalNotice) -> Result<(), CallbackError> {
        if let Some(publisher) = &self.publisher {
            publisher.update(fr_client::robot::live::Observation::approval).map_err(|()| CallbackError)?;
        }
        Ok(())
    }
    fn ready(&mut self, _: u8, desktop: &mut Desktop) -> Result<(), CallbackError> {
        self.receipt_time.set(report::unix_ms());
        if let Some(publisher) = &self.publisher {
            let original = Observation::initial_from(desktop).ok_or(CallbackError)?;
            if self.minimum == Minimum::SubmittedToCompositor && original.completion.stage != PresentationStage::SubmittedToCompositor {
                self.insufficient.set(true);
                return Err(CallbackError);
            }
            publisher.update(|state| state.ready(original.display, original.completion.frame.as_raw(), evidence(original.completion.stage), Instant::now()))
                .map_err(|()| CallbackError)?;
        }
        Ok(())
    }
    fn frame(&mut self, _: u8, frame: Option<Presentation>) -> Result<(), CallbackError> {
        if let (Some(publisher), Some(frame)) = (&self.publisher, frame) {
            publisher.update(|state| state.presented(frame.frame.as_raw(), evidence(frame.stage), Instant::now()))
                .map_err(|()| CallbackError)?;
        }
        Ok(())
    }
    fn status(&mut self, status: Status) -> Result<(), CallbackError> {
        let Some(publisher) = &self.publisher else { return Ok(()); };
        publisher.update(|state| match status {
            Status::Connecting { attempt } => state.begin(attempt),
            Status::Authenticated { .. } => state.authenticated(),
            Status::Cleaning { .. } => { state.retire(Phase::Cleaning); Ok(()) }
            Status::Reconnecting { .. } => { state.retire(Phase::Reconnecting); Ok(()) }
            Status::Completed { .. } | Status::Stopped { .. } => { state.retire(Phase::Stopped); Ok(()) }
        }).map_err(|()| CallbackError)
    }
}

struct Shutdown { interrupt: Signal, terminate: Signal, hangup: Signal }
impl Shutdown {
    fn new() -> Result<Self, Failure> {
        let open = |kind| signal(kind).map_err(|_| failure("signal_unavailable", "Resolve local signal registration before observation."));
        Ok(Self { interrupt: open(SignalKind::interrupt())?, terminate: open(SignalKind::terminate())?, hangup: open(SignalKind::hangup())? })
    }
    async fn requested(&mut self) {
        let mut interrupt = pin!(self.interrupt.recv());
        let mut terminate = pin!(self.terminate.recv());
        let mut hangup = pin!(self.hangup.recv());
        poll_fn(|task| {
            if interrupt.as_mut().poll(task).is_ready() || terminate.as_mut().poll(task).is_ready() || hangup.as_mut().poll(task).is_ready() {
                Poll::Ready(())
            } else { Poll::Pending }
        }).await;
    }
}
/// Listener failure is not authenticated stop intent. Keep signals usable.
async fn local_or_signal(local: impl Future<Output = Result<(), session_control::Error>>, signal: impl Future<Output = ()>) {
    let mut local = pin!(local);
    let mut signal = pin!(signal);
    let mut failed = false;
    poll_fn(|task| {
        if signal.as_mut().poll(task).is_ready() { return Poll::Ready(()); }
        if !failed {
            match local.as_mut().poll(task) {
                Poll::Ready(Ok(())) => return Poll::Ready(()),
                Poll::Ready(Err(_)) => failed = true,
                Poll::Pending => {}
            }
        }
        Poll::Pending
    }).await;
}
/// Signals and local STOP cancel, but never abandon the native cleanup future.
async fn drain_on_shutdown<T>(cx: &Cx, stopped: &Cell<bool>, operation: impl Future<Output = T>, shutdown: impl Future<Output = ()>) -> T {
    let mut operation = pin!(operation);
    let mut shutdown = pin!(shutdown);
    poll_fn(|task| {
        if !stopped.get() && shutdown.as_mut().poll(task).is_ready() {
            stopped.set(true);
            cx.cancel_fast(CancelKind::User);
        }
        operation.as_mut().poll(task)
    }).await
}
#[cfg(test)]
#[path = "completion_tests.rs"]
mod tests;
