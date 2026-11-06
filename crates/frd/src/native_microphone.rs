//! The host application's explicitly enabled microphone on one original
//! controlled session. A separate, responsive local Stop microphone surface
//! is required before the role-8 offer and throughout native output. The
//! surface cannot grant control or activate the client's microphone.
use crate::{
    host_indicator::microphone::{Owner, Status},
    input_process::ProcessLaunch,
    microphone_process::{ChildExit, Launch, Retirement},
    microphone_quic::{self as uplink, Error},
    session_startup::ControlledHost,
    worker::{self, Deadline},
};
use asupersync::cx::Cx;
use fr_transport::quic::ChannelRequest;
use fr_wire::{attachment::Ticket, decoder::Binding, negotiation::ControlBinding};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

const SETUP: Duration = Duration::from_secs(2);

/// Local installation and endpoint choices; neither path comes from a peer.
/// Supplying this profile records the host operator's separate microphone
/// enable. Actual admission still needs a live surface and the original grant.
#[derive(Debug, Clone)]
pub struct Configuration {
    launch: Launch,
    indicator: ProcessLaunch,
}
impl Configuration {
    pub const fn new(launch: Launch, indicator: ProcessLaunch) -> Self {
        Self { launch, indicator }
    }
}

struct Local {
    enabled: AtomicBool,
    native: Mutex<Option<uplink::WeakControl>>,
}
impl Local {
    fn stop(&self) {
        self.enabled.store(false, Ordering::Release);
        // Permission predicates can detect UI loss inside transport dispatch.
        // The native supervisor independently checks enabled, so contention
        // never waits, grants output, or loses the terminal privacy decision.
        if let Ok(native) = self.native.try_lock()
            && let Some(native) = native.as_ref()
        {
            native.request_stop();
        }
    }
    fn bind(&self, native: uplink::Control) {
        *self
            .native
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(native.downgrade());
        if !self.enabled.load(Ordering::Acquire) {
            self.stop();
        }
    }
}

pub(crate) struct Application {
    launch: Option<Launch>,
    surface: Owner,
    local: Arc<Local>,
    offered: bool,
    bound: bool,
    retiring: Option<Retirement>,
}
impl Application {
    /// Start the supervised privacy surface before the first control grant.
    /// This starts no device or codec, and keeps setup off the authority task.
    pub(crate) fn new(config: Configuration) -> Result<Self, Error> {
        let local = Arc::new(Local {
            enabled: AtomicBool::new(true),
            native: Mutex::new(None),
        });
        let stopping = local.clone();
        let surface = Owner::start(config.indicator, move || stopping.stop())
            .map_err(|_| Error::Permission)?;
        Ok(Self {
            launch: Some(config.launch),
            surface,
            local,
            offered: false,
            bound: false,
            retiring: None,
        })
    }
    pub(crate) fn close(&mut self) {
        self.local.stop();
        self.surface.stop();
        self.launch = None;
    }
    /// Between complete network turns. A control-lease renewal proves the
    /// client has promoted its original controller and installed expectations.
    pub(crate) fn host(
        &mut self,
        host: &mut ControlledHost,
        view: Binding,
        nonce: &mut impl FnMut() -> Result<u128, ()>,
    ) -> Result<(), Error> {
        self.observe(host);
        let status = self.surface.control().status();
        if !self.local.enabled.load(Ordering::Acquire) || matches!(status, Status::Stopped(_)) {
            self.local.stop();
            if self.offered {
                host.retire_microphone()?;
                self.observe(host);
                return Ok(());
            }
            // A positively selected client must not wait forever for an offer
            // that cannot be made. No native microphone has started here.
            return Err(Error::Permission);
        }
        if self.offered || status != Status::Ready || host.control_renewed_until().is_none() {
            return Ok(());
        }
        let id = host.next_channel_binding().map_err(|_| Error::Binding)?;
        let ticket = nonce().map_err(|()| Error::Binding)?;
        if id == 0 || ticket == 0 {
            return Err(Error::Binding);
        }
        let surface = self.surface.control();
        let local = self.local.clone();
        let permission: Arc<dyn Fn() -> bool + Send + Sync> = Arc::new(move || {
            local.enabled.load(Ordering::Acquire) && surface.status() == Status::Ready
        });
        let launch = self.launch.take().ok_or(Error::Closed)?;
        host.offer_microphone_with_permission(
            ChannelRequest {
                binding: Binding {
                    parent: ControlBinding { id, ..view.parent },
                    ..view
                },
                ticket: Ticket(ticket),
                timeout: SETUP,
            },
            true,
            launch,
            Some(permission),
        )?;
        self.offered = true;
        Ok(())
    }
    fn observe(&mut self, host: &ControlledHost) {
        if !self.bound
            && let Some(native) = host.microphone_control()
        {
            self.local.bind(native);
            self.bound = true;
        }
        if let Some(retiring) = host.microphone_retirement() {
            self.retiring = Some(retiring);
        }
    }
    /// Keep both exact native-child receipts and the UI worker through cleanup.
    /// A timeout retains this Application for another reap; requesting stop is
    /// never reported as a proven device or codec exit.
    pub(crate) async fn reap(
        &mut self,
        host: Option<&ControlledHost>,
        cx: &Cx,
        deadline: Deadline,
    ) -> Result<(), worker::Error> {
        if let Some(host) = host {
            self.observe(host);
        }
        self.close();
        loop {
            cx.checkpoint().map_err(|_| worker::Error::Cancelled)?;
            let now = cx
                .timer_driver()
                .ok_or(worker::Error::MissingRuntime)?
                .now();
            if now >= deadline.time() {
                return Err(worker::Error::ReapPending);
            }
            let native = self.retiring.as_ref().is_none_or(Retirement::is_complete);
            let surface = self.surface.try_finish();
            if native && let Some(surface) = surface {
                surface.map_err(|_| worker::Error::Unavailable)?;
                if let Some(retiring) = &self.retiring {
                    let exits = retiring.receipts();
                    if [exits.device, exits.codec]
                        .into_iter()
                        .any(|exit| !matches!(exit, ChildExit::NotStarted | ChildExit::Reaped))
                    {
                        return Err(worker::Error::ReapPending);
                    }
                }
                return Ok(());
            }
            asupersync::time::sleep(now, Duration::from_millis(5)).await;
        }
    }
}
impl Drop for Application {
    fn drop(&mut self) {
        self.close();
    }
}
