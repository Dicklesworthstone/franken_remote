//! Native-application clipboard ownership. Configure once on the original native
//! publisher/observer; its service starts negotiation only after control exists.
//! Factories run on the clipboard thread, never inside UI or QUIC callbacks.
use crate::{
    clipboard_quic::{Error, WorkerSeed, WorkerTask},
    session_startup::{ControlledHost, ControlledViewer},
    worker::Deadline,
};
use asupersync::cx::Cx;
use fr_core::clipboard::{ClipboardSwitch, PlatformError};
use fr_transport::quic::ChannelRequest;
use fr_wire::{
    attachment::Ticket,
    clipboard::session::synchronize::{IdentifierFailure, NativeClipboard, Received},
    decoder::Binding,
};
use std::{
    fmt,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

type Launch = Box<dyn FnOnce(WorkerSeed) -> Result<WorkerTask, Error> + Send>;

/// Separate native OS permission from the UI's enabled switch. Supplying this
/// configuration does not confer control, select a protocol profile or read text.
/// Neither factory nor item-ID source is called before bilateral readiness.
pub struct Configuration {
    timeout: Duration,
    consent: bool,
    launch: Option<Launch>,
}
impl Configuration {
    pub fn new<N, F, I>(
        consent: bool,
        timeout: Duration,
        factory: F,
        new_id: I,
    ) -> Result<Self, Error>
    where
        N: NativeClipboard + 'static,
        F: FnOnce() -> Result<N, PlatformError> + Send + 'static,
        I: FnMut() -> Result<u128, IdentifierFailure> + Send + 'static,
    {
        if timeout.is_zero() || timeout > Duration::from_secs(2) || timeout.as_micros() == 0 {
            return Err(Error::Limit);
        }
        Ok(Self {
            timeout,
            consent,
            launch: Some(Box::new(move |seed| seed.spawn(factory, new_id))),
        })
    }
}
impl fmt::Debug for Configuration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("NativeClipboardConfiguration([local permission and native factory])")
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    WaitingForControl,
    Negotiating,
    Running,
    Retired,
    Closed,
}
/// Stopping admission does NOT prove a potentially blocked foreign call ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cleanup {
    NotStarted,
    Pending,
    Finished(Result<(), Error>),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Status {
    pub phase: Phase,
    pub enabled: bool,
    pub cleanup: Cleanup,
    pub reason: Option<Error>,
}
struct State {
    status: Status,
    stopped: bool,
    switch: Option<ClipboardSwitch>,
    stop: Option<crate::clipboard_quic::WorkerControl>,
    received: [Option<Received>; 2],
}
/// Content-free UI handle for THIS original native session. The switch defaults
/// on, but cannot override consent, a dead input grant or a terminal stop. A clone
/// never retains/replaces input authority and cannot cause reconnection/retry.
#[derive(Clone)]
pub struct Control(Arc<Mutex<State>>);
impl fmt::Debug for Control {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("NativeClipboardControl")
            .field(&self.status())
            .finish()
    }
}
impl Control {
    fn lock(&self) -> MutexGuard<'_, State> {
        // No caller-supplied code or native work runs under this short lock.
        // Poisoning fences both original owners before exposing recovered status.
        self.0.lock().unwrap_or_else(|poison| {
            let mut s = poison.into_inner();
            s.stopped = true;
            s.status.enabled = false;
            s.status.reason = Some(Error::Poisoned);
            if let Some(switch) = &s.switch {
                switch.set_enabled(false);
            }
            if let Some(stop) = &s.stop {
                stop.stop();
            }
            s
        })
    }
    pub fn status(&self) -> Status {
        self.lock().status
    }
    /// May be called by an independent local UI thread. Disabling the actual
    /// switch immediately fences final publication, including during native prep.
    /// Transport retirement is serviced by the original session's next drive.
    pub fn set_enabled(&self, enabled: bool) -> Result<(), Error> {
        let mut s = self.lock();
        if s.stopped || matches!(s.status.phase, Phase::Retired | Phase::Closed) {
            return Err(Error::Closed);
        }
        s.status.enabled = enabled;
        if let Some(switch) = &s.switch {
            switch.set_enabled(enabled);
        }
        Ok(())
    }
    /// Terminal for clipboard only. Before control/negotiation, explicitly
    /// declines readiness without any native open. During incomplete attachment,
    /// existing conservative session cancellation still applies. A completed
    /// optional lane retires in isolation.
    pub fn stop(&self) {
        let mut s = self.lock();
        s.stopped = true;
        s.status.enabled = false;
        if let Some(switch) = &s.switch {
            switch.set_enabled(false);
        }
        if let Some(stop) = &s.stop {
            stop.stop();
        }
    }
    /// One live receipt plus at most one final in-flight result after stop, not
    /// text history. Both remain collectable after the native owner is reaped.
    pub fn take_received(&self) -> Option<Received> {
        let mut s = self.lock();
        let result = s.received[0].take();
        s.received[0] = s.received[1].take();
        result
    }
}

pub(crate) struct Application {
    config: Configuration,
    control: Control,
    task: Option<WorkerTask>,
    started: bool,
    declining: bool,
}
impl Application {
    pub(crate) fn new(config: Configuration) -> Self {
        Self {
            config,
            task: None,
            started: false,
            declining: false,
            control: Control(Arc::new(Mutex::new(State {
                status: Status {
                    phase: Phase::WaitingForControl,
                    enabled: true,
                    cleanup: Cleanup::NotStarted,
                    reason: None,
                },
                stopped: false,
                switch: None,
                stop: None,
                received: [None; 2],
            }))),
        }
    }
    /// Positive profile negotiation is not a native application configuration.
    /// Participate with metadata-only refusal so a configured peer is not left
    /// waiting for its original setup deadline. There is deliberately no factory.
    /// Call only while beginning a new native control-acquisition service, never
    /// to replace a pre-attached lane on an already controlled session.
    pub(crate) fn decline_unconfigured(slot: &mut Option<Self>, profile_selected: bool) {
        if slot.is_none() && profile_selected {
            *slot = Some(Self::new(Configuration {
                timeout: Duration::from_secs(2),
                consent: false,
                launch: None,
            }));
        }
    }
    pub(crate) fn control(&self) -> Control {
        self.control.clone()
    }
    pub(crate) fn close(&mut self) {
        self.control.stop();
        self.config.launch = None;
        self.control.lock().status.phase = Phase::Closed;
        self.collect_cleanup();
    }
    fn collect_cleanup(&mut self) {
        if let Some(task) = &mut self.task
            && let Some(result) = task.finish()
        {
            self.control.lock().status.cleanup = Cleanup::Finished(result);
            self.task = None;
        }
    }
    /// Keep the original thread handle if cleanup times out, so a later cleanup
    /// can still observe completion. No blocking join and no reset timeout.
    pub(crate) async fn reap(&mut self, cx: &Cx, deadline: Deadline) -> Result<Cleanup, Error> {
        self.close();
        loop {
            self.collect_cleanup();
            if self.task.is_none() {
                return Ok(self.control.status().cleanup);
            }
            cx.checkpoint().map_err(|_| Error::Cancelled)?;
            let now = cx.timer_driver().ok_or(Error::Clock)?.now();
            if now >= deadline.time() {
                return Err(Error::HandoffExpired);
            }
            asupersync::time::sleep(now, Duration::from_millis(1)).await;
        }
    }
    pub(crate) fn host(
        &mut self,
        host: &mut ControlledHost,
        view: Binding,
        nonce: &mut impl FnMut() -> Result<u128, ()>,
    ) -> Result<(), Error> {
        self.turn(host, |host, timeout, consent| {
            let id = host
                .io()
                .map_err(|_| Error::Closed)?
                .0
                .next_channel_binding()
                .map_err(Error::Transport)?;
            let ticket = nonce().map_err(|()| Error::Limit)?;
            if id == 0 || ticket == 0 {
                return Err(Error::Limit);
            }
            host.offer_clipboard(
                ChannelRequest {
                    binding: Binding {
                        parent: fr_wire::negotiation::ControlBinding { id, ..view.parent },
                        ..view
                    },
                    ticket: Ticket(ticket),
                    timeout,
                },
                consent,
            )
        })
    }
    pub(crate) fn viewer(&mut self, viewer: &mut ControlledViewer) -> Result<(), Error> {
        self.turn(viewer, ControlledViewer::expect_clipboard)
    }
    fn turn<E: Endpoint>(
        &mut self,
        endpoint: &mut E,
        begin: impl FnOnce(&mut E, Duration, bool) -> Result<(), Error>,
    ) -> Result<(), Error> {
        self.collect_cleanup();
        self.collect_received(endpoint)?;
        let (stopped, enabled) = {
            let state = self.control.lock();
            (state.stopped, state.status.enabled)
        };
        if stopped && self.started && !self.declining {
            endpoint.retire()?;
            self.retired(None);
            return Ok(());
        }
        if !self.started {
            self.started = true; // An error/panic never retries IDs, consent or setup.
            // A local choice made before control must not strand the peer's
            // optional handshake. Exchange only declined readiness metadata;
            // this path can never release a seed or open either clipboard.
            self.declining = stopped || !enabled || !self.config.consent;
            match begin(endpoint, self.config.timeout, !self.declining) {
                Ok(()) => self.control.lock().status.phase = Phase::Negotiating,
                Err(Error::NotNegotiated) => {
                    self.retired(Some(Error::NotNegotiated));
                    return Ok(());
                }
                Err(e) => {
                    self.retired(Some(e));
                    return Err(e);
                }
            }
        }
        if endpoint.retired() {
            self.retired(endpoint.reason());
            return Ok(());
        }
        if let Some(seed) = endpoint.take_worker() {
            let stop = seed.control();
            let (local, _) = endpoint.switches().ok_or(Error::Closed)?;
            {
                let mut s = self.control.lock();
                local.set_enabled(s.status.enabled && !s.stopped);
                s.switch = Some(local);
                s.stop = Some(stop.clone());
                if s.stopped {
                    stop.stop();
                }
            }
            if self.control.lock().stopped {
                drop(seed);
                endpoint.retire()?;
                self.retired(None);
                return Ok(());
            }
            let launch = self.config.launch.take().ok_or(Error::AlreadyAttached)?;
            match launch(seed) {
                Ok(task) => {
                    self.task = Some(task);
                    let mut s = self.control.lock();
                    s.status.phase = Phase::Running;
                    s.status.cleanup = Cleanup::Pending;
                }
                Err(e) => {
                    stop.stop();
                    endpoint.retire()?;
                    self.retired(Some(e));
                }
            }
        }
        Ok(())
    }
    fn retired(&mut self, reason: Option<Error>) {
        self.control.stop();
        self.config.launch = None;
        let mut s = self.control.lock();
        s.status.phase = Phase::Retired;
        if s.status.reason.is_none() {
            s.status.reason = reason;
        }
    }
    pub(crate) fn collect_received<E: Endpoint>(&mut self, endpoint: &mut E) -> Result<(), Error> {
        let space = {
            let s = self.control.lock();
            s.received[0].is_none() || (s.stopped && s.received[1].is_none())
        };
        if space && let Some(receipt) = endpoint.received()? {
            // Only this application produces results; UI consumers can only free
            // space while the endpoint is sampled. Never hold a UI lock there.
            let mut s = self.control.lock();
            *s.received
                .iter_mut()
                .find(|slot| slot.is_none())
                .ok_or(Error::Limit)? = Some(receipt);
        }
        Ok(())
    }
}
impl Drop for Application {
    fn drop(&mut self) {
        self.close();
    }
}

pub(crate) trait Endpoint {
    fn retire(&mut self) -> Result<(), Error>;
    fn retired(&self) -> bool;
    fn reason(&self) -> Option<Error>;
    fn take_worker(&mut self) -> Option<WorkerSeed>;
    fn switches(&self) -> Option<(ClipboardSwitch, ClipboardSwitch)>;
    fn received(&mut self) -> Result<Option<Received>, Error>;
}
macro_rules! endpoint {
    ($t:ty) => {
        impl Endpoint for $t {
            fn retire(&mut self) -> Result<(), Error> {
                self.retire_clipboard()
            }
            fn retired(&self) -> bool {
                self.clipboard_retired()
            }
            fn reason(&self) -> Option<Error> {
                self.clipboard_reason()
            }
            fn take_worker(&mut self) -> Option<WorkerSeed> {
                self.take_clipboard_worker()
            }
            fn switches(&self) -> Option<(ClipboardSwitch, ClipboardSwitch)> {
                self.clipboard_switches()
            }
            fn received(&mut self) -> Result<Option<Received>, Error> {
                self.take_clipboard_received()
            }
        }
    };
}
endpoint!(ControlledHost);
endpoint!(ControlledViewer);
