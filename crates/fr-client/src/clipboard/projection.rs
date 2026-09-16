//! Original-client lifetime and conservative host-clock authority projection.
use super::{Error, ProjectedClock};
use crate::input::ClientInstant;
use fr_core::{
    clipboard::{
        Binding,
        authority::{Authority, Monitor},
    },
    input_submission::Refusal,
    time::HostInstant,
};
use fr_media::freshness::{ClockCorrelation, ReceiverLifetime};
use fr_wire::{control::Granted, negotiation::ControlBinding};
use std::sync::{
    Arc, Mutex, Weak,
    atomic::{AtomicBool, Ordering},
};

pub(crate) struct Owner {
    state: Arc<State>,
    attached: bool,
}
impl Drop for Owner {
    fn drop(&mut self) {
        // An in-flight weak upgrade may temporarily retain State, but can never
        // retain the original owner's authority past its destruction.
        self.stop();
    }
}
struct Metadata {
    receiver: Option<ReceiverLifetime>,
    clock: ClockCorrelation,
    last_client: u64,
    last_host: u64,
    host_until: u64,
    local_until: u64,
    mapped: bool,
    view_until: Option<u64>,
    obligations_until: u64,
}
pub(super) struct State {
    pub(super) stopped: AtomicBool,
    binding: Binding,
    parent: ControlBinding,
    display_channel: u32,
    input_channel: u32,
    inner: Mutex<Metadata>,
}
impl Owner {
    /// Called ONLY after `RequestControl` has correlated and validated the actual
    /// grant and its initial ticket. `InputClient::new` alone cannot reach here.
    pub(crate) fn new(
        g: Granted,
        clock: ClockCorrelation,
        now: ClientInstant,
    ) -> Result<Self, Error> {
        let host = clock.age_upper_us(0, now.0).map_err(|_| Error::Clock)?;
        let local_until = clock
            .deadline_lower_us(g.lease_until_us, now.0)
            .map_err(|_| Error::Clock)?;
        if clock.host_boot() != g.request.parent.host_boot
            || now.0 >= local_until
            || host >= g.lease_until_us
        {
            return Err(Error::Expired);
        }
        Ok(Self {
            attached: false,
            state: Arc::new(State {
                stopped: AtomicBool::new(false),
                binding: Binding {
                    session: g.request.parent.remote_session,
                    lease: g.lease,
                },
                parent: g.request.parent,
                display_channel: g.request.target.display_binding,
                input_channel: g.input_channel,
                inner: Mutex::new(Metadata {
                    receiver: None,
                    clock,
                    last_client: now.0,
                    last_host: host,
                    host_until: g.lease_until_us,
                    local_until,
                    mapped: false,
                    view_until: None,
                    obligations_until: u64::MAX,
                }),
            }),
        })
    }
    /// Installed once by the presented-input join, before an auxiliary worker
    /// can escape. A new receiver requires a new grant, never equal numeric IDs.
    pub(crate) fn bind_receiver(&self, lifetime: ReceiverLifetime) {
        if let Ok(mut inner) = self.state.inner.lock() {
            if inner.receiver.is_some() || !lifetime.is_live() {
                self.stop();
            } else {
                inner.receiver = Some(lifetime);
            }
        } else {
            self.stop();
        }
    }
    pub(crate) fn parent(&self) -> ControlBinding {
        self.state.parent
    }
    pub(crate) fn binding(&self) -> Binding {
        self.state.binding
    }
    pub(crate) fn stop(&self) {
        self.state.stopped.store(true, Ordering::Release);
    }
    pub(crate) fn check(&self, now: ClientInstant) -> Result<(), Error> {
        self.state.sample(now, false).map(|_| ())
    }
    pub(crate) fn readiness(
        &self,
        mapped: bool,
        view_until: Option<ClientInstant>,
        obligations_until: u64,
    ) {
        if let Ok(mut inner) = self.state.inner.lock() {
            inner.mapped = mapped;
            inner.view_until = view_until.map(|at| at.0);
            inner.obligations_until = obligations_until;
        } else {
            self.stop();
        }
    }
    /// A queued control response is NOT evidence of host acceptance. Only a
    /// validated, newer authenticated ticket proves the host lease is live at
    /// least until that ticket's deadline. Never shorten the initial lease to
    /// the ticket lifetime, nor revive an already expired original owner.
    pub(crate) fn ticket(
        &self,
        host_until: u64,
        clock: ClockCorrelation,
        now: ClientInstant,
    ) -> Result<(), Error> {
        self.check(now)?;
        let result = (|| {
            let mut m = self.state.inner.lock().map_err(|_| Error::Stopped)?;
            if clock.host_boot() != m.clock.host_boot()
                || clock.received_at_us() < m.clock.received_at_us()
            {
                return Err(Error::Clock);
            }
            let host = clock
                .age_upper_us(0, now.0)
                .map_err(|_| Error::Clock)?
                .max(m.last_host);
            let host_until = host_until.max(m.host_until);
            let local_until = clock
                .deadline_lower_us(host_until, now.0)
                .map_err(|_| Error::Clock)?;
            if now.0 >= local_until || host >= host_until {
                return Err(Error::Expired);
            }
            m.clock = clock;
            m.host_until = host_until;
            m.local_until = local_until;
            m.last_host = host;
            m.last_client = now.0;
            Ok(())
        })();
        if result.is_err() {
            self.stop();
        }
        result
    }
    pub(super) fn attach(
        &mut self,
        channel: u32,
        now: ClientInstant,
    ) -> Result<(Monitor, ProjectedClock), Error> {
        if self.attached {
            return Err(Error::AlreadyAttached);
        }
        if channel == 0
            || [
                self.state.parent.id,
                self.state.display_channel,
                self.state.input_channel,
            ]
            .contains(&channel)
        {
            return Err(Error::WrongChannel);
        }
        self.state.sample(now, true)?;
        // Consume before exposing any owner. Failed setup cannot reset the
        // per-lease replay ledger by constructing another clipboard channel.
        self.attached = true;
        let weak = Arc::downgrade(&self.state);
        Ok((
            Monitor::new(Projection {
                state: weak.clone(),
                binding: self.state.binding,
            }),
            ProjectedClock {
                state: weak,
                fallback: HostInstant::from_micros(
                    self.state
                        .inner
                        .lock()
                        .map_err(|_| Error::Stopped)?
                        .last_host,
                ),
            },
        ))
    }
}
impl State {
    fn failure<T>(&self, error: Error) -> Result<T, Error> {
        self.stopped.store(true, Ordering::Release);
        Err(error)
    }
    pub(super) fn sample(
        &self,
        now: ClientInstant,
        require_view: bool,
    ) -> Result<HostInstant, Error> {
        if self.stopped.load(Ordering::Acquire) {
            return Err(Error::Stopped);
        }
        let mut m = self.inner.lock().map_err(|_| {
            self.stopped.store(true, Ordering::Release);
            Error::Stopped
        })?;
        if m.receiver
            .as_ref()
            .is_some_and(|receiver| !receiver.is_live())
        {
            return self.failure(Error::Stopped);
        }
        if now.0 < m.last_client {
            return self.failure(Error::Clock);
        }
        if now.0 >= m.local_until
            || now.0 >= m.obligations_until
            || m.view_until.is_some_and(|at| now.0 >= at)
        {
            return self.failure(Error::Expired);
        }
        // A better correlation may lower its uncertainty, but must not make the
        // accounting clock stand still and thereby extend an in-flight item's
        // fixed lifetime. Advance at least by elapsed client time, while keeping
        // the current correlation's full conservative host upper bound.
        let Some(rolling) = m.last_host.checked_add(now.0 - m.last_client) else {
            return self.failure(Error::Clock);
        };
        let host = match m.clock.age_upper_us(0, now.0) {
            Ok(host) => host.max(rolling),
            Err(_) => return self.failure(Error::Clock),
        };
        if host >= m.host_until {
            return self.failure(Error::Expired);
        }
        m.last_client = now.0;
        m.last_host = host;
        if require_view && (!m.mapped || m.view_until.is_none()) {
            return Err(Error::NotReady);
        }
        // An independent local stop takes priority even while the metadata lock
        // is held. No grant/clock/presentation update clears this bit.
        if self.stopped.load(Ordering::Acquire) {
            return Err(Error::Stopped);
        }
        Ok(HostInstant::from_micros(host))
    }
    pub(super) fn last_host(&self) -> Option<HostInstant> {
        self.inner
            .lock()
            .ok()
            .map(|m| HostInstant::from_micros(m.last_host))
    }
}
struct Projection {
    state: Weak<State>,
    binding: Binding,
}
impl Authority for Projection {
    fn binding(&self) -> Binding {
        self.binding
    }
    fn revoke(&self) {
        if let Some(state) = self.state.upgrade() {
            state.stopped.store(true, Ordering::Release);
        }
    }
    fn deadline(&self, now: HostInstant) -> Result<HostInstant, Refusal> {
        let state = self.state.upgrade().ok_or(Refusal::Revoked)?;
        if state.stopped.load(Ordering::Acquire) {
            return Err(Refusal::Revoked);
        }
        let m = state
            .inner
            .lock()
            .map_err(|_| Refusal::AuthorityUnavailable)?;
        // The facade samples the client clock immediately before EVERY policy
        // check, including after native preparation. Arbitrary host timestamps
        // are not interchangeable with that qualified projection.
        if m.receiver
            .as_ref()
            .is_some_and(|receiver| !receiver.is_live())
            || now.as_micros() != m.last_host
            || !m.mapped
            || m.view_until.is_none()
            || m.view_until.is_some_and(|at| m.last_client >= at)
            || m.last_client >= m.local_until
            || m.last_client >= m.obligations_until
            || m.last_host >= m.host_until
            || state.stopped.load(Ordering::Acquire)
        {
            state.stopped.store(true, Ordering::Release);
            return Err(Refusal::Revoked);
        }
        Ok(HostInstant::from_micros(m.host_until))
    }
}
