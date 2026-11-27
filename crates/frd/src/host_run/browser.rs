//! Installed, view-only browser desktop over two independent WSS sockets.
//! The protected listener, original admission, capture receipt and native
//! session monitor remain owned until their explicit cleanup has completed.
mod connection;
mod factory;
mod listener;

use super::{
    Encoder, EncoderSelection, Error, Event, Options, Reporter, StopHandle, Watch, check, policy,
    select_encoder, session_ended, session_monitor, session_opened, session_setup, signals,
    unless_stopped,
};
use crate::{
    broker::browser_assets::BrowserAssets,
    browser_ingress::{BrowserIngress, BrowserIngressConfig},
    session_monitor::{Monitor, Status},
};
use asupersync::{
    cx::Cx,
    runtime::{RuntimeBuilder, RuntimeHandle},
    types::{Budget, CancelKind},
};
use fr_core::{
    ids::{HostBootId, OsSessionId},
    limits::ProtocolLimits,
};
use fr_tailnet::{CertificatePolicy, LocalApi};
use std::{
    future::Future,
    pin::pin,
    sync::{Arc, Mutex},
    task::Poll,
    time::Duration,
};

/// Validate the effective API profile as well as the CLI selection. Direct
/// callers cannot silently request native effects on a browser connection.
fn check_profile(options: &Options, port: u16) -> Result<(), Error> {
    check(options)?;
    if port == 0
        || options.session_monitor.is_none()
        || options.input_agent.is_some()
        || options.clipboard
        || options.audio.is_some()
        || options.microphone.is_some()
        || options.files.is_some()
        || options.file_send.is_some()
        || options.directory
        || options.adaptive_capture
    {
        return Err(Error::Configuration);
    }
    if !BrowserAssets::bundled() {
        return Err(Error::BrowserTransport("client_bundle"));
    }
    Ok(())
}

/// Enter through host_indicator::run_browser: the actual local Stop sharing
/// surface must already be ready and remain alive through this run's cleanup.
pub(crate) fn run(
    options: &Options,
    port: u16,
    report: &Reporter,
    stop: &Arc<StopHandle>,
    configuration: policy::Configuration,
    selection: EncoderSelection,
) -> Result<(), Error> {
    check_profile(options, port)?;
    if stop.is_requested() {
        return Ok(());
    }
    let Some(encoder) = select_encoder(options, selection, stop)? else {
        return Ok(());
    };
    if selection == EncoderSelection::AutomaticHardware {
        report(Event::EncoderSelected { encoder });
    }
    let monitor = Monitor::start(
        options
            .session_monitor
            .clone()
            .ok_or(Error::Configuration)?,
    )
    .map_err(Error::SessionMonitor)?;
    let lifetime = monitor.control();
    let result = serve(
        options,
        port,
        report,
        stop,
        configuration,
        encoder,
        &lifetime,
    );
    let ended = session_ended(Some(&lifetime));
    let cleaned = finish_monitor(monitor);
    if cleaned.is_err() {
        report(Event::CleanupFailed {
            stage: "session_monitor",
        });
    }
    cleaned?;
    match (result, ended) {
        (Ok(()), Some(cause)) => Err(Error::SessionEnded(cause)),
        (Ok(()), None) => {
            report(Event::Stopped);
            Ok(())
        }
        (Err(error), _) => Err(error),
    }
}

fn finish_monitor(mut monitor: Monitor) -> Result<(), Error> {
    monitor.stop();
    let until = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        match monitor.try_finish() {
            Some(Ok(())) => return Ok(()),
            Some(Err(_)) => return Err(Error::Cleanup("session_monitor")),
            None if std::time::Instant::now() < until => {
                std::thread::sleep(Duration::from_millis(5));
            }
            None => return Err(Error::Cleanup("session_monitor")),
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn serve(
    options: &Options,
    port: u16,
    report: &Reporter,
    stop: &Arc<StopHandle>,
    configuration: policy::Configuration,
    encoder: Encoder,
    lifetime: &session_monitor::Control,
) -> Result<(), Error> {
    let runtime = RuntimeBuilder::new()
        .worker_threads(2)
        .enable_platform_reactor(true)
        .build()
        .map_err(|_| Error::Runtime)?;
    let broker = runtime.request_cx_with_budget(Budget::INFINITE);
    let renewal = runtime.request_cx_with_budget(Budget::INFINITE);
    let cleanup = runtime.request_cx_with_budget(Budget::INFINITE);
    let service = runtime.request_cx_with_budget(Budget::INFINITE);
    let handle = runtime.handle();
    let active = Mutex::new(Some(service.clone()));
    runtime.block_on(async {
        let mut policy = policy::Owner::start(&broker, Some(configuration))?;
        let result = async {
            let mut handled = signals(options.handle_signals);
            let mut signals = Watch::new(&mut handled);
            let Some(opened) =
                unless_stopped(session_opened(&broker, lifetime), stop, &mut signals, None).await
            else {
                return Ok(());
            };
            opened?;
            let Some(ready) = unless_stopped(
                session_setup(
                    &broker,
                    Some(lifetime),
                    policy.ready_with_approval(&broker, false),
                ),
                stop,
                &mut signals,
                Some(lifetime),
            )
            .await
            else {
                return Ok(());
            };
            ready??;
            let epoch =
                policy::lease_with_approval(policy.handle(), false)?.ok_or(Error::Configuration)?;
            let gate = Gate {
                cx: service,
                stop: stop.clone(),
                lifetime: lifetime.clone(),
                policy: epoch,
            };
            gate.check()?;
            let api = match &options.socket {
                Some(path) => LocalApi::new(path).map_err(Error::Tailnet)?,
                None => LocalApi::installed(),
            };
            let roots =
                fr_tailnet::trust::root_store(&options.trust_roots).map_err(Error::Trust)?;
            report(Event::ObtainingCertificate);
            let Some(identity) = unless_stopped(
                session_setup(
                    &broker,
                    Some(lifetime),
                    api.native_server_identity(&broker, roots, CertificatePolicy::default()),
                ),
                stop,
                &mut signals,
                Some(lifetime),
            )
            .await
            else {
                return Ok(());
            };
            let identity = identity?.map_err(Error::Tailnet)?;
            let result = async {
                gate.check()?;
                let node = api.node_identity(&broker).await.map_err(Error::Tailnet)?;
                let config = BrowserIngressConfig::https_tailnet(
                    node.certificate_name(),
                    port,
                    HostBootId::from_raw(super::random_nonzero_u128()?),
                    OsSessionId::from_raw(super::random_nonzero_u128()?),
                    ProtocolLimits::ABSOLUTE,
                );
                let ingress = Arc::new(
                    BrowserIngress::new(config)
                        .with_desktop(factory::make(options, encoder, &handle, &gate)),
                );
                let renew = identity.serve_renewal(&renewal).map_err(Error::Tailnet)?;
                // The supervisor fences the original service Cx on credential/local
                // failure and keeps polling the listener through explicit cleanup.
                super::supervise(
                    async { renew.await.map_err(Error::Tailnet) },
                    listener::serve(
                        options, port, report, &handle, &api, &identity, node, ingress, gate,
                        &broker, &cleanup,
                    ),
                    &active,
                    stop,
                    |task| super::local_stops(task, &mut signals, Some(lifetime), stop),
                )
                .await
            }
            .await;
            identity.stop();
            result
        }
        .await;
        let cleaned = policy.finish(&cleanup).await;
        cleaned?;
        result
    })
}

/// The original run and saved-policy epoch. All resumed connection polls and
/// capture supervision consult this same owner; none adopt a newer policy.
#[derive(Clone)]
struct Gate {
    cx: Cx,
    stop: Arc<StopHandle>,
    lifetime: session_monitor::Control,
    policy: crate::host_policy::live::Lease,
}
impl Gate {
    fn stopped(&self) -> bool {
        self.stop.is_requested() || self.cx.is_cancel_requested()
    }
    fn check(&self) -> Result<(), Error> {
        if self.stopped() {
            return Err(Error::BrowserTransport("stopped"));
        }
        match self.lifetime.status() {
            Status::Active => {}
            Status::Stopped(cause) => return Err(Error::SessionEnded(cause)),
            Status::Opening => {
                return Err(Error::SessionMonitor(session_monitor::Error::Selection));
            }
        }
        self.policy.check().map_err(Error::Policy)?;
        Ok(())
    }
    fn register(&self, task: &std::task::Context<'_>) {
        self.stop.register(task.waker());
        self.lifetime.register(task);
    }
    async fn watch(self) -> Result<(), ()> {
        loop {
            let mut tick = pin!(asupersync::time::sleep(
                self.cx.now(),
                Duration::from_millis(20)
            ));
            std::future::poll_fn(|task| {
                self.register(task);
                if self.check().is_err() {
                    return Poll::Ready(Err(()));
                }
                tick.as_mut().poll(task).map(Ok)
            })
            .await?;
        }
    }
}

fn context(runtime: &RuntimeHandle) -> Result<Cx, Error> {
    runtime
        .try_request_cx_with_budget(Budget::INFINITE)
        .map_err(|_| Error::Runtime)
}

/// Dropping a TCP connection always retires that original admission context.
/// Capture cleanup is retained separately by BrowserIngress.
struct ContextGuard(Cx);
impl Drop for ContextGuard {
    fn drop(&mut self) {
        self.0.cancel_fast(CancelKind::User);
    }
}
