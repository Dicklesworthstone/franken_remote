//! Read-only, original-owner authorization carried across an async handoff.
use super::{Context, Error, HostInstant, ProtocolLimits, SessionError};
use fr_core::clipboard::{
    Binding, ClipboardSwitch,
    authority::{Authority, Monitor},
};
use fr_core::input_submission::Refusal;
use std::{
    fmt,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

/// Retains the original monitor, not a replacement grant. Channel destruction
/// fences every clone. Native calls must never be made while checking this.
#[derive(Clone)]
pub struct Transport {
    monitor: Monitor,
    live: Arc<AtomicBool>,
    context: Context,
    limits: ProtocolLimits,
    local: ClipboardSwitch,
    peer: ClipboardSwitch,
}
impl fmt::Debug for Transport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ClipboardTransport([original owner])")
    }
}
impl Transport {
    pub(super) fn new(
        monitor: Monitor,
        live: Arc<AtomicBool>,
        context: Context,
        limits: ProtocolLimits,
        local: ClipboardSwitch,
        peer: ClipboardSwitch,
    ) -> Self {
        Self {
            monitor,
            live,
            context,
            limits,
            local,
            peer,
        }
    }
    pub const fn context(&self) -> Context {
        self.context
    }
    pub const fn limits(&self) -> ProtocolLimits {
        self.limits
    }
    pub fn switches(&self) -> (ClipboardSwitch, ClipboardSwitch) {
        (self.local.clone(), self.peer.clone())
    }
    /// Authority only: release-only cancellations can travel while disabled.
    /// `now` MUST be sampled in the original owner's qualified clock domain.
    pub fn is_open(&self) -> bool {
        self.live.load(Ordering::Acquire)
    }
    pub fn check(&self, now: HostInstant) -> Result<(), SessionError> {
        if !self.live.load(Ordering::Acquire) {
            return Err(Error::Closed.into());
        }
        self.monitor
            .deadline(now)
            .map_err(|e| SessionError::Clipboard(Error::Authority(e)))?;
        if !self.live.load(Ordering::Acquire) {
            return Err(Error::Closed.into());
        }
        Ok(())
    }
    /// Fence clipboard work only. Never revoke the input monitor.
    pub fn close(&self) {
        self.live.store(false, Ordering::Release);
    }
}

/// Exact operation deadline and cancellation state, without retaining text.
/// Cloning cannot extend the deadline or survive the original channel's close.
#[derive(Clone)]
pub struct Egress {
    transport: Transport,
    deadline: HostInstant,
    payload_live: Option<Arc<AtomicBool>>,
    switches: (u64, u64),
}
impl fmt::Debug for Egress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ClipboardEgress([bounded original operation])")
    }
}
impl Egress {
    pub(super) fn new(
        transport: Transport,
        deadline: HostInstant,
        payload_live: Option<Arc<AtomicBool>>,
    ) -> Self {
        let switches = (transport.local.state(), transport.peer.state());
        Self {
            transport,
            deadline,
            payload_live,
            switches,
        }
    }
    pub const fn deadline(&self) -> HostInstant {
        self.deadline
    }
    pub const fn context(&self) -> Context {
        self.transport.context()
    }
    pub fn check(&self, now: HostInstant) -> Result<(), SessionError> {
        self.transport.check(now)?;
        self.check_operation(now)
    }
    /// Deadline, lifetime and cancellation ONLY, not an authority check. A
    /// qualified controller handoff checks its own original grant/projection
    /// before this, without racing the native worker's independent clock cursor.
    pub fn check_operation(&self, now: HostInstant) -> Result<(), SessionError> {
        if !self.transport.is_open() {
            return Err(Error::Closed.into());
        }
        if now >= self.deadline {
            return Err(Error::Expired.into());
        }
        if let Some(live) = &self.payload_live {
            if !live.load(Ordering::Acquire) {
                return Err(Error::LocalChanged.into());
            }
            if !self.transport.local.is_enabled()
                || !self.transport.peer.is_enabled()
                || self.switches != (self.transport.local.state(), self.transport.peer.state())
            {
                return Err(Error::Disabled.into());
            }
        }
        Ok(())
    }
}

// Put the handoff lifetime in the core's final native-publication check too.
// A network close during slow preparation cannot leave the native side usable.
pub(super) fn lifetime(original: Monitor) -> (Monitor, Arc<AtomicBool>) {
    let live = Arc::new(AtomicBool::new(true));
    (
        Monitor::new(Lifetime {
            original,
            live: live.clone(),
        }),
        live,
    )
}
struct Lifetime {
    original: Monitor,
    live: Arc<AtomicBool>,
}
impl Authority for Lifetime {
    fn binding(&self) -> Binding {
        self.original.binding()
    }
    fn deadline(&self, now: HostInstant) -> Result<HostInstant, Refusal> {
        if !self.live.load(Ordering::Acquire) {
            return Err(Refusal::Revoked);
        }
        let deadline = self.original.deadline(now)?;
        if !self.live.load(Ordering::Acquire) {
            return Err(Refusal::Revoked);
        }
        Ok(deadline)
    }
    fn revoke(&self) {
        self.live.store(false, Ordering::Release);
        self.original.revoke();
    }
}
