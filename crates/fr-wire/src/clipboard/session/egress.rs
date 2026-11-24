//! Original-owner authorization and release-only expiry across an async handoff.
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
        atomic::{AtomicBool, AtomicU8, Ordering},
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
        self.deadline(now).map(drop)
    }
    /// Read the original owner's exclusive authorization cap. A caller which
    /// waits here must resample its clock afterward and retain this exact cap.
    pub fn deadline(&self, now: HostInstant) -> Result<HostInstant, SessionError> {
        if !self.live.load(Ordering::Acquire) {
            return Err(Error::Closed.into());
        }
        let deadline = self.monitor.deadline(now);
        if !self.live.load(Ordering::Acquire) {
            return Err(Error::Closed.into());
        }
        deadline.map_err(Self::refusal)
    }
    /// Refusal-only native fence after a fresh clock sample. The caller must
    /// already retain and check the original authorization and item deadlines;
    /// this nonblocking check cannot grant or replace either bound. A viewer
    /// must use the exact cursor which authorized this monitor's sample.
    pub fn check_publication(&self, now: HostInstant) -> Result<(), SessionError> {
        if !self.live.load(Ordering::Acquire) {
            return Err(Error::Closed.into());
        }
        let refusal = self.monitor.publication_refusal(now);
        if !self.live.load(Ordering::Acquire) {
            return Err(Error::Closed.into());
        }
        refusal.map_or(Ok(()), |reason| Err(Self::refusal(reason)))
    }
    fn refusal(reason: Refusal) -> SessionError {
        SessionError::Clipboard(
            if reason == Refusal::Authority(fr_core::authority::AuthorityError::ViewUnready) {
                Error::Suspended
            } else {
                Error::Authority(reason)
            },
        )
    }
    /// Fence clipboard work only. Never revoke the input monitor.
    pub fn close(&self) {
        self.live.store(false, Ordering::Release);
    }
}

// One state belongs to the original item, not a numeric stamp or a clock
// projection. An unsent handoff can shorten its lifetime; it cannot renew it.
pub(super) struct Payload(AtomicU8);
impl Payload {
    const ACTIVE: u8 = 0;
    const EXPIRED: u8 = 1;
    const CANCELLED: u8 = 2;
    const COMMITTED: u8 = 3;
    const COMMITTED_CANCELLED: u8 = 4;

    pub(super) fn new() -> Self {
        Self(AtomicU8::new(Self::ACTIVE))
    }
    pub(super) fn invalidate(&self) {
        let _ = self
            .0
            .try_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                Some(
                    if matches!(state, Self::COMMITTED | Self::COMMITTED_CANCELLED) {
                        Self::COMMITTED_CANCELLED
                    } else {
                        Self::CANCELLED
                    },
                )
            });
    }
    pub(super) fn is_expired(&self) -> bool {
        self.0.load(Ordering::Acquire) == Self::EXPIRED
    }
    pub(super) fn complete(&self) -> bool {
        // Never overwrite a concurrent release fence with completion.
        self.0
            .compare_exchange(
                Self::ACTIVE,
                Self::COMMITTED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }
    fn check(&self) -> Result<(), Error> {
        match self.0.load(Ordering::Acquire) {
            Self::ACTIVE | Self::COMMITTED => Ok(()),
            // Earlier records already admitted to transport keep their own
            // immutable deadlines and ordered cancellation, like superseded
            // items. Only the wire owner interprets the item's expiry signal.
            _ => Err(Error::LocalChanged),
        }
    }
    fn expire_unsent(&self) -> bool {
        match self.0.compare_exchange(
            Self::ACTIVE,
            Self::EXPIRED,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) | Err(Self::EXPIRED | Self::CANCELLED) => true,
            Err(_) => false,
        }
    }
}

/// Exact operation deadline and cancellation state, without retaining text.
/// Cloning cannot extend the deadline or survive the original channel's close.
#[derive(Clone)]
pub struct Egress {
    transport: Transport,
    deadline: HostInstant,
    payload_live: Option<Arc<Payload>>,
    expirable_unsent: bool,
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
        payload_live: Option<Arc<Payload>>,
        expirable_unsent: bool,
    ) -> Self {
        let switches = (transport.local.state(), transport.peer.state());
        Self {
            transport,
            deadline,
            payload_live,
            expirable_unsent,
            switches,
        }
    }
    pub const fn deadline(&self) -> HostInstant {
        self.deadline
    }
    pub const fn context(&self) -> Context {
        self.transport.context()
    }
    /// Retire this exact item's expired, wholly unsubmitted Begin or Chunk.
    /// The trusted handoff owner must still own all bytes and have observed an
    /// original host-clock or local handoff deadline. This release-only signal
    /// cannot extend a deadline, admit bytes, or affect a replacement item. The
    /// original wire owner emits its ordered Cancel on its next maintenance
    /// turn, even when its independent clock projection has not reached expiry.
    /// Commit, Cancel and already completed items cannot use this path.
    pub fn expire_unsent_payload(&self) -> bool {
        self.expirable_unsent
            && self
                .payload_live
                .as_ref()
                .is_some_and(|payload| payload.expire_unsent())
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
            live.check()?;
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
    fn is_revoked(&self) -> bool {
        !self.live.load(Ordering::Acquire) || self.original.is_revoked()
    }
    fn publication_refusal(&self, now: HostInstant) -> Option<Refusal> {
        if !self.live.load(Ordering::Acquire) {
            return Some(Refusal::Revoked);
        }
        let refusal = self.original.publication_refusal(now);
        if !self.live.load(Ordering::Acquire) {
            return Some(Refusal::Revoked);
        }
        refusal
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
    fn item_deadline(&self, now: HostInstant) -> Result<HostInstant, Refusal> {
        if !self.live.load(Ordering::Acquire) {
            return Err(Refusal::Revoked);
        }
        let deadline = self.original.item_deadline(now)?;
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
