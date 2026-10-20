//! Original-client lifetime and conservative host-clock authority projection.
pub(crate) mod files;
use super::{Error, ProjectedClock};
use crate::input::{ClientInstant, MAX_VIEW_SUSPENSION_US};
use fr_core::{
    authority::AuthorityError,
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

/// One monotonically advancing cursor per policy consumer. Network checks
/// cannot replace the sample the native owner is about to validate.
#[derive(Clone, Copy)]
pub(super) struct Cursor {
    pub(super) client: u64,
    pub(super) host: u64,
}

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
    last_view_until: Option<u64>,
    paused_since: Option<u64>,
    resume_ticket: bool,
    reference_recovery: bool,
}
impl Metadata {
    fn pause(&mut self, since: u64) {
        if self.paused_since.is_none() {
            self.resume_ticket = false;
        }
        self.paused_since = Some(self.paused_since.map_or(since, |old| old.min(since)));
    }
    fn resume_if_ready(&mut self, now: u64) {
        if self.resume_ticket
            && now < self.local_until
            && self.last_host < self.host_until
            && self.mapped
            && self.view_until.is_some_and(|until| now < until)
            && self.receiver.as_ref().is_none_or(ReceiverLifetime::is_live)
        {
            self.paused_since = None;
            self.resume_ticket = false;
        }
    }
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
                    last_view_until: None,
                    paused_since: None,
                    resume_ticket: false,
                    reference_recovery: false,
                }),
            }),
        })
    }
    /// Installed once by the presented-input join, before an auxiliary worker
    /// can escape. A replacement is accepted only by the validated recovery join.
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
    /// Only the presented-input owner can allow its negotiated recovery to
    /// pause a fenced receiver. A lost receiver grants no clipboard permission.
    pub(crate) fn expect_reference_recovery(&self) {
        if let Ok(mut inner) = self.state.inner.lock() {
            inner.reference_recovery = true;
        } else {
            self.stop();
        }
    }
    /// Called after the same presented-input grant validated the actual media
    /// receiver lineage, unchanged configuration and newer recovery generation.
    /// Numeric IDs alone never reach this join, and the old lifetime stays dead.
    pub(crate) fn follow_recovery(&self, lifetime: ReceiverLifetime) {
        if let Ok(mut inner) = self.state.inner.lock() {
            if !inner.reference_recovery
                || inner.paused_since.is_none()
                || inner
                    .receiver
                    .as_ref()
                    .is_none_or(|receiver| receiver.is_live() || !receiver.is_owner_live())
                || !lifetime.is_live()
            {
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
    /// Refresh only the authenticated clock model. The previous model must
    /// still be valid; neither a late sample nor a better correlation revives
    /// this owner, renews lease proof, resumes a view or moves an item deadline.
    pub(crate) fn synchronize(
        &self,
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
            m.clock = clock;
            m.last_host = host;
            // Keep local_until, host_until, resume_ticket and paused_since.
            // The rolling host cursor preserves all previously started bounds.
            self.state.ready_at(&mut m, now.0, host, false)
        })();
        if result.is_err() {
            self.stop();
        }
        result
    }
    pub(crate) fn readiness(
        &self,
        mapped: bool,
        view_until: Option<ClientInstant>,
        obligations_until: u64,
        suspended_since: Option<ClientInstant>,
    ) {
        if let Ok(mut inner) = self.state.inner.lock() {
            // A late owner tick must retain the old view's actual deadline
            // before the input owner replaces it with a suspended view.
            let lapse = suspended_since
                .map(|since| inner.view_until.map_or(since.0, |until| until.min(since.0)));
            inner.mapped = mapped;
            inner.view_until = view_until.map(|at| at.0);
            if let Some(until) = view_until {
                inner.last_view_until = Some(until.0);
            }
            inner.obligations_until = obligations_until;
            if let Some(since) = lapse {
                inner.pause(since);
            } else {
                let now = inner.last_client;
                inner.resume_if_ready(now);
            }
        } else {
            self.stop();
        }
    }
    /// A queued control response is NOT evidence of host acceptance. Only a
    /// validated, newer authenticated ticket proves the host lease is live at
    /// least until that ticket's deadline. Never shorten the initial lease to
    /// the ticket lifetime. A proof that lapsed during suspension can be replaced;
    /// a terminal owner cannot. Only the input owner's validated post-lapse
    /// ticket gate sets `resume_ticket`; a control response is not proof.
    pub(crate) fn ticket(
        &self,
        host_until: u64,
        clock: ClockCorrelation,
        now: ClientInstant,
        resume_ticket: bool,
    ) -> Result<(), Error> {
        self.refresh_proof(host_until, clock, now, resume_ticket, false)
    }
    /// The negotiated profile carries the original host owner's actual
    /// lease/observation cap, rather than only a lower bound from a short input
    /// ticket. Honor a shorter cap too. The old owner must still be live, and
    /// existing items, native reads and handoffs retain their original deadlines.
    pub(crate) fn authority_ticket(
        &self,
        host_until: u64,
        clock: ClockCorrelation,
        now: ClientInstant,
        resume_ticket: bool,
    ) -> Result<(), Error> {
        self.refresh_proof(host_until, clock, now, resume_ticket, true)
    }
    fn refresh_proof(
        &self,
        host_until: u64,
        clock: ClockCorrelation,
        now: ClientInstant,
        resume_ticket: bool,
        exact: bool,
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
            let host_until = if exact {
                host_until
            } else {
                host_until.max(m.host_until)
            };
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
            if m.paused_since.is_some() {
                m.resume_ticket |= resume_ticket;
                m.resume_if_ready(now.0);
            }
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
        allow_paused: bool,
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
        let at = self.state.sample(now, !allow_paused)?;
        if allow_paused {
            let inner = self.state.inner.lock().map_err(|_| Error::Stopped)?;
            // Attachment may finish during an already observed view's lapse.
            // It never turns an unmapped or never-presented grant into a view.
            if !inner.mapped || inner.last_view_until.is_none() {
                return Err(Error::NotReady);
            }
        }
        let cursor = Arc::new(Mutex::new(Cursor {
            client: now.0,
            host: at.as_micros(),
        }));
        // Consume before exposing any owner. Failed setup cannot reset the
        // per-lease replay ledger by constructing another clipboard channel.
        self.attached = true;
        let weak = Arc::downgrade(&self.state);
        Ok((
            Monitor::new(Projection {
                state: weak.clone(),
                binding: self.state.binding,
                cursor: cursor.clone(),
            }),
            ProjectedClock {
                state: weak,
                cursor,
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
        if now.0 < m.last_client {
            return self.failure(Error::Clock);
        }
        self.lifecycle(&mut m, now.0)?;
        // A tighter correlation cannot stop the accounting clock and thereby
        // extend an in-flight item's original fixed lifetime.
        let Some(rolling) = m.last_host.checked_add(now.0 - m.last_client) else {
            return self.failure(Error::Clock);
        };
        let host = match m.clock.age_upper_us(0, now.0) {
            Ok(host) => host.max(rolling),
            Err(_) => return self.failure(Error::Clock),
        };
        m.last_client = now.0;
        m.last_host = host;
        self.ready_at(&mut m, now.0, host, require_view)?;
        Ok(HostInstant::from_micros(host))
    }
    // The original input client advances Metadata; each clipboard consumer
    // advances its own cursor. Overtaking is conservative, while regression
    // within any one consumer remains a terminal clock fault.
    pub(super) fn sample_cursor(
        &self,
        now: ClientInstant,
        cursor: &Mutex<Cursor>,
        require_view: bool,
    ) -> Result<HostInstant, Error> {
        let result = self.sample_cursor_inner(now, cursor, require_view);
        if result.as_ref().is_err_and(|e| *e != Error::NotReady) {
            self.stopped.store(true, Ordering::Release);
        }
        result
    }
    fn sample_cursor_inner(
        &self,
        now: ClientInstant,
        cursor: &Mutex<Cursor>,
        require_view: bool,
    ) -> Result<HostInstant, Error> {
        let mut c = cursor.lock().map_err(|_| Error::Stopped)?;
        let mut m = self.inner.lock().map_err(|_| Error::Stopped)?;
        if now.0 < c.client {
            return self.failure(Error::Clock);
        }
        let local = now.0.max(m.last_client);
        self.lifecycle(&mut m, local)?;
        let rolling = c.host.checked_add(now.0 - c.client).ok_or(Error::Clock)?;
        let base = m
            .last_host
            .checked_add(local - m.last_client)
            .ok_or(Error::Clock)?;
        let host = m
            .clock
            .age_upper_us(0, local)
            .map_err(|_| Error::Clock)?
            .max(rolling)
            .max(base);
        // Paused samples still advance this cursor, including maintenance
        // clocks. No fresh-looking fallback clock can extend a retained item.
        *c = Cursor {
            client: now.0,
            host,
        };
        self.ready_at(&mut m, local, host, require_view)?;
        Ok(HostInstant::from_micros(host))
    }
    fn lifecycle(&self, m: &mut Metadata, local: u64) -> Result<(), Error> {
        if self.stopped.load(Ordering::Acquire) {
            return self.failure(Error::Stopped);
        }
        let lost_receiver = m.receiver.as_ref().is_some_and(|r| !r.is_live());
        if m.receiver.as_ref().is_some_and(|r| !r.is_owner_live())
            || (lost_receiver && !m.reference_recovery)
        {
            return self.failure(Error::Stopped);
        }
        if local >= m.obligations_until {
            return self.failure(Error::Expired);
        }
        // Lapse time never slides with later polls, tickets or clock samples.
        // A worker can meet a fenced receiver before the input owner does.
        let lapse = m
            .view_until
            .filter(|until| local >= *until)
            .or_else(|| lost_receiver.then_some(local));
        if let Some(since) = lapse {
            m.pause(since);
        }
        if m.paused_since
            .is_some_and(|since| local.saturating_sub(since) >= MAX_VIEW_SUSPENSION_US)
        {
            return self.failure(Error::Expired);
        }
        Ok(())
    }
    fn ready_at(
        &self,
        m: &mut Metadata,
        local: u64,
        host: u64,
        require_view: bool,
    ) -> Result<(), Error> {
        self.lifecycle(m, local)?;
        // The last ticket proves a lower bound on the host's current lease.
        // During suspension that proof may lapse, but grants no permission;
        // only a newer authenticated ticket can replace it.
        if (local >= m.local_until || host >= m.host_until) && m.paused_since.is_none() {
            return self.failure(Error::Expired);
        }
        if require_view && (!m.mapped || m.view_until.is_none() || m.paused_since.is_some()) {
            return Err(Error::NotReady);
        }
        Ok(())
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
    cursor: Arc<Mutex<Cursor>>,
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
        let c = self
            .cursor
            .lock()
            .map_err(|_| Refusal::AuthorityUnavailable)?;
        let mut m = state
            .inner
            .lock()
            .map_err(|_| Refusal::AuthorityUnavailable)?;
        // This exact sample belongs to the native consumer. An original-owner
        // update can only tighten its decision; no lock spans native work.
        let local = c.client.max(m.last_client);
        if now.as_micros() != c.host {
            state.stopped.store(true, Ordering::Release);
            return Err(Refusal::Revoked);
        }
        let status = (|| {
            state.lifecycle(&mut m, local)?;
            let host = m.clock.age_upper_us(0, local).map_err(|_| Error::Clock)?;
            state.ready_at(&mut m, local, host.max(c.host), true)
        })();
        match status {
            Ok(()) => {}
            Err(Error::NotReady) => return Err(Refusal::Authority(AuthorityError::ViewUnready)),
            Err(_) => {
                state.stopped.store(true, Ordering::Release);
                return Err(Refusal::Revoked);
            }
        }
        if state.stopped.load(Ordering::Acquire) {
            return Err(Refusal::Revoked);
        }
        Ok(HostInstant::from_micros(m.host_until))
    }
}
