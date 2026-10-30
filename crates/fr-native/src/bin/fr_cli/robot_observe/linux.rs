//! One view-only attempt through the ordinary native supervisor and cleanup.
use super::{Failure, Request, report};
use asupersync::{
    cx::Cx,
    runtime::RuntimeBuilder,
    signal::{Signal, SignalKind, signal},
    types::{Budget, CancelKind},
};
use fr_native::desktop::{
    Configuration as DesktopConfiguration, Desktop,
    reconnect::{Session, Ui},
};
use fr_wire::display::Catalog;
use frd::{
    native_connection::{
        AddressFamily, Client, Configuration, LocalApi, PeerSelector, TailnetError,
        reconnect::{self, CallbackError},
    },
    session_startup::ObserverPolicy,
};
use std::{
    cell::Cell,
    future::{Future, poll_fn},
    path::PathBuf,
    pin::pin,
    rc::Rc,
    task::Poll,
    time::Duration,
};

fn failure(code: &'static str, next: &'static str) -> Failure {
    Failure::new(code, next, 1)
}

#[allow(clippy::too_many_lines)] // One attempt, its mandatory cleanup, then reporting.
pub(super) fn run(request: &Request) -> Result<String, Failure> {
    let mut shutdown = Shutdown::new()?;
    let runtime = RuntimeBuilder::current_thread()
        .enable_platform_reactor(true)
        .build()
        .map_err(|_| failure("runtime_unavailable", "Check local runtime resources."))?;
    let cx = runtime
        .handle()
        .try_request_cx_with_budget(Budget::INFINITE)
        .map_err(|_| failure("runtime_unavailable", "Check local runtime resources."))?;
    let connection = &request.connection;
    let roots = fr_tailnet::trust::read_certificates(&connection.target.roots)
        .map_err(|_| invalid_roots())?;
    if roots.len() > 256 {
        return Err(invalid_roots());
    }
    let api = match &request.socket {
        Some(path) => LocalApi::new(path).map_err(tailnet)?,
        None => LocalApi::installed(),
    };
    let mut client = Client::new(api, roots, Duration::from_secs(5)).map_err(|_| {
        failure(
            "invalid_native_configuration",
            "Check the local trust store and installed-tailnet native configuration.",
        )
    })?;
    let display = connection
        .x_display
        .clone()
        .or_else(|| std::env::var("DISPLAY").ok())
        .ok_or_else(|| {
            failure(
                "display_unavailable",
                "Set DISPLAY or --x-display for an existing local X11 session; this observation is not headless.",
            )
        })?;
    let xauthority = std::env::var_os("XAUTHORITY").map(PathBuf::from);
    let mut epoch = [0; 16];
    cx.random_bytes(&mut epoch);
    let mut desktop = DesktopConfiguration::new(
        &connection.worker,
        &display,
        xauthority.as_deref(),
        u128::from_be_bytes(epoch),
    )
    .map_err(|_| {
        failure(
            "invalid_worker_configuration",
            "Select a trusted locally installed worker and matching local graphical-session settings.",
        )
    })?;
    if let Some((width, height)) = connection.fit_window {
        desktop = desktop.with_fitted_window(width, height).map_err(|_| {
            failure(
                "invalid_window_size",
                "Choose bounded even dimensions for --fit.",
            )
        })?;
    }
    let receipt_time = Rc::new(Cell::new(None));
    let missing_display = Rc::new(Cell::new(false));
    let mut application = Session::observing_once(
        desktop,
        ObserverPolicy::default(),
        Interface {
            display: connection.display,
            receipt_time: receipt_time.clone(),
            missing_display: missing_display.clone(),
        },
    );
    let selector = if connection.target.by_name {
        PeerSelector::Name(&connection.target.node)
    } else {
        PeerSelector::StableId(&connection.target.node)
    };
    let operation = client.run_observing(
        cx.clone(),
        runtime.handle(),
        selector,
        Configuration {
            port: connection.target.port,
            family: if connection.target.ipv6 {
                AddressFamily::Ipv6
            } else {
                AddressFamily::Automatic
            },
            ..Default::default()
        },
        fr_client::native::observation_offer(),
        reconnect::Policy {
            max_attempts: 1,
            ..Default::default()
        },
        &mut application,
    );
    let stopped = Cell::new(false);
    let result = runtime.block_on(drain_on_shutdown(
        &cx,
        &stopped,
        operation,
        shutdown.requested(),
    ));
    let result = completion_gate(
        result,
        application.desktop().is_none() && application.cleanup_failure().is_none(),
        stopped.get(),
        missing_display.get(),
    )
    .and_then(|()| {
        report::render(
            request,
            application.initial_observation(),
            receipt_time.get(),
            application.disconnect_outcome(),
        )
    });
    // A later local failure must not discard an actually received host report.
    crate::closure::preserve(result, application.disconnect_outcome())
}

fn invalid_roots() -> Failure {
    failure(
        "invalid_trust_store",
        "Use a locally provisioned regular non-symlink PEM root file, at most 1 MiB and 256 certificates. Never use roots supplied by the contacted peer.",
    )
}

fn completion_gate(
    result: Result<(), reconnect::Failure>,
    native_clean: bool,
    stopped: bool,
    missing_display: bool,
) -> Result<(), Failure> {
    if !native_clean
        || matches!(
            result,
            Err(reconnect::Failure::Cleanup | reconnect::Failure::CleanupExpired)
        )
    {
        return Err(failure(
            "native_cleanup_incomplete",
            "The observation ended without confirmed native cleanup. Inspect remaining workers before another connection.",
        ));
    }
    if stopped {
        return Err(Failure::new(
            "cancelled",
            "One-shot observation was stopped and its supervisor completed local cleanup; no actions were replayed.",
            130,
        ));
    }
    if missing_display {
        return Err(failure(
            "display_not_offered",
            "The requested display is not in the approved catalog; --display only requires exactly one display. Inspect fr displays before another observation.",
        ));
    }
    result.map_err(|error| match error {
        reconnect::Failure::Cancelled => Failure::new("cancelled", "Observation cancelled.", 130),
        reconnect::Failure::Connection(frd::native_connection::Error::Tailnet(error)) => {
            tailnet(error)
        }
        _ => failure(
            "native_observation_failed",
            "Native observation or its closing exchange failed. Check local display/worker permissions and host approval; no retry or screenshot was performed.",
        ),
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
    failure(
        code,
        "Check installed Tailscale, the exact discovered host, host service and local trusted roots. Never disable identity or certificate checks.",
    )
}

struct Interface {
    display: crate::options::DisplayChoice,
    receipt_time: Rc<Cell<Option<u64>>>,
    missing_display: Rc<Cell<bool>>,
}
impl Ui for Interface {
    fn choose(&mut self, _: u8, catalog: &Catalog) -> Result<Option<u128>, CallbackError> {
        let selected = self.display.select(catalog);
        if selected.is_none() {
            self.missing_display.set(true);
            return Err(CallbackError);
        }
        Ok(selected)
    }
    fn approval(&mut self, _: u8, _: fr_client::startup::ApprovalNotice) -> Result<(), CallbackError> {
        Ok(())
    }
    fn ready(&mut self, _: u8, _: &mut Desktop) -> Result<(), CallbackError> {
        self.receipt_time.set(report::unix_ms());
        Ok(())
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
                    "Resolve local signal registration before observation.",
                )
            })
        };
        Ok(Self {
            interrupt: open(SignalKind::interrupt())?,
            terminate: open(SignalKind::terminate())?,
            hangup: open(SignalKind::hangup())?,
        })
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
}

/// Cancel the supervisor, never drop its future: its independent native reap
/// and the actual cleanup result must finish even after a signal wins the race.
async fn drain_on_shutdown<T>(
    cx: &Cx,
    stopped: &Cell<bool>,
    operation: impl Future<Output = T>,
    shutdown: impl Future<Output = ()>,
) -> T {
    let mut operation = pin!(operation);
    let mut shutdown = pin!(shutdown);
    poll_fn(|task| {
        if !stopped.get() && shutdown.as_mut().poll(task).is_ready() {
            stopped.set(true);
            cx.cancel_fast(CancelKind::User);
        }
        operation.as_mut().poll(task)
    })
    .await
}

#[cfg(test)]
#[path = "completion_tests.rs"]
mod tests;
