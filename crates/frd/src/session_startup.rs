//! Native session startup on the existing authenticated Asupersync control pair.
//! Admission owns the clock. Consent and `BindingAccepted` precede exposure of
//! observation authority; startup never opens a decoder or grants input.
use crate::media::ObservationControl;
use asupersync::{
    cx::Cx,
    net::quic_native::{NativeQuicUdpConnection, StreamRole},
};
use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    time::HostInstant,
};
use fr_tailnet::Admission;
use fr_transport::quic::{
    self, ConnectionBinding, ControlRoutes, Disposition, Policy, QuicRecords, Route,
};
use fr_wire::negotiation::{self, ControlBinding, Message, Offer, Role, Selection};
use std::{
    cell::Cell,
    fmt,
    sync::{
        Arc, Weak,
        atomic::{AtomicU8, Ordering},
    },
    time::Duration,
};

const WAITING: u8 = 1;
const ALLOWED: u8 = 2;
const DENIED: u8 = 3;
const RETIRED: u8 = 4;
const CONSUMED: u8 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    InvalidConfiguration,
    Admission(fr_tailnet::Error),
    Protocol(negotiation::Error),
    Transport(quic::Error),
    Authority,
    Order,
    Denied,
    Expired,
    Clock,
    Cancelled,
    Closed,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Error {}
impl From<quic::Error> for Error {
    fn from(e: quic::Error) -> Self {
        Self::Transport(e)
    }
}
impl From<negotiation::Error> for Error {
    fn from(e: negotiation::Error) -> Self {
        Self::Protocol(e)
    }
}
fn now(cx: &Cx) -> Result<u64, Error> {
    cx.checkpoint().map_err(|_| Error::Cancelled)?;
    Ok(cx.timer_driver().ok_or(Error::Clock)?.now().as_nanos() / 1000)
}

/// Locally configured values, never peer-provided authority. Boot/OS/session IDs
/// must be freshly allocated by the qualified host owner, not a wire request.
#[derive(Debug, Clone)]
pub struct Configuration {
    pub offer: Offer,
    pub binding: ControlBinding,
    pub require_approval: bool,
    pub startup_timeout: Duration,
    pub authority: AuthorityPolicy,
    pub transport: Policy,
}
impl Configuration {
    fn validate(&self) -> Result<u64, Error> {
        self.offer.validate()?;
        let timeout = u64::try_from(self.startup_timeout.as_micros())
            .map_err(|_| Error::InvalidConfiguration)?;
        let life = self.authority.authorization_lifetime.as_micros();
        let ticket = self.authority.ticket_lifetime.as_micros();
        if timeout == 0
            || timeout > 60_000_000
            || self.binding.id == 0
            || self.binding.host_boot.as_raw() == 0
            || self.binding.os_session.as_raw() == 0
            || self.binding.remote_session.as_raw() == 0
            || life == 0
            || life > 3_000_000
            || ticket == 0
            || ticket > life
        {
            return Err(Error::InvalidConfiguration);
        }
        Ok(timeout)
    }
}

/// One local consent capability. It neither keeps the session alive nor applies
/// to another owner with equal numeric IDs. There is no network approval RPC.
#[derive(Clone)]
pub struct Approval {
    state: Weak<AtomicU8>,
    cx: Cx,
    deadline: u64,
}
impl fmt::Debug for Approval {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Approval([local one-use decision])")
    }
}
impl Approval {
    pub fn decide(&self, allow: bool) -> Result<(), Error> {
        let state = self.state.upgrade().ok_or(Error::Closed)?;
        if now(&self.cx)? >= self.deadline {
            return Err(Error::Expired);
        }
        let decision = if allow { ALLOWED } else { DENIED };
        state
            .compare_exchange(WAITING, decision, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| Error::Order)?;
        Ok(())
    }
}

enum Peer {
    Tailnet(Admission),
    #[cfg(test)]
    Fixture {
        alive: Arc<std::sync::atomic::AtomicBool>,
        until: u64,
        control: bool,
    },
}
impl Peer {
    fn check(&self, cx: &Cx, role: Role) -> Result<u64, Error> {
        let current = now(cx)?;
        match self {
            Self::Tailnet(owner) => {
                let lease = owner.lease();
                let deadline = if role == Role::RequestControl {
                    lease.control()
                } else {
                    lease.observe()
                }
                .map_err(Error::Admission)?;
                if current >= deadline {
                    return Err(Error::Expired);
                }
                Ok(deadline)
            }
            #[cfg(test)]
            Self::Fixture {
                alive,
                until,
                control,
            } => {
                if !alive.load(Ordering::Acquire) {
                    return Err(Error::Denied);
                }
                if role == Role::RequestControl && !control {
                    return Err(Error::Denied);
                }
                if current >= *until {
                    return Err(Error::Expired);
                }
                Ok(*until)
            }
        }
    }
    fn check_addresses(&self, transport: &QuicRecords) -> Result<(), Error> {
        if transport.role()? != StreamRole::Server {
            return Err(Error::Order);
        }
        match self {
            Self::Tailnet(owner) => {
                let a = owner.lease().addresses();
                if transport.addresses()? != (a.local, a.peer) {
                    return Err(Error::Order);
                }
            }
            #[cfg(test)]
            Self::Fixture { .. } => {}
        }
        Ok(())
    }
    fn observation(
        &self,
        cx: Cx,
        authority: SessionAuthority,
    ) -> Result<ObservationControl, Error> {
        match self {
            Self::Tailnet(owner) => ObservationControl::new_admitted(cx, authority, owner.lease()),
            #[cfg(test)]
            Self::Fixture { .. } => ObservationControl::new(cx, authority),
        }
        .map_err(|_| Error::Authority)
    }
    fn revoke(&self) {
        match self {
            Self::Tailnet(owner) => owner.revoke(),
            #[cfg(test)]
            Self::Fixture { alive, .. } => {
                alive.store(false, Ordering::Release);
            }
        }
    }
    async fn refresh(&mut self) -> Result<(), Error> {
        match self {
            Self::Tailnet(owner) => owner.refresh().await.map_err(Error::Admission),
            #[cfg(test)]
            Self::Fixture { .. } => Ok(()),
        }
    }
}
struct Permit<'a> {
    peer: &'a Peer,
    cx: &'a Cx,
    role: Role,
    until: u64,
    decision: &'a AtomicU8,
}
impl Permit<'_> {
    fn check(&self) -> bool {
        !matches!(self.decision.load(Ordering::Acquire), DENIED | RETIRED)
            && now(self.cx).is_ok_and(|n| n < self.until)
            && self.peer.check(self.cx, self.role).is_ok()
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Hello,
    Selection,
    Approval,
    Bind,
    Ack,
    Complete,
    Closed,
    Detached,
}

/// One startup, one connection, one admission refresh owner, one pending record.
/// Its caller must continue `drive` while waiting for consent or network traffic.
/// No detached tasks are spawned. Dropping an active drive is terminal.
pub struct Host {
    cx: Cx,
    peer: Option<Peer>,
    transport: Option<QuicRecords>,
    routes: ControlRoutes,
    config: Configuration,
    intersection: Option<Offer>,
    selected: Option<Selection>,
    authority: Option<SessionAuthority>,
    phase: Phase,
    role: Role,
    approval: Arc<AtomicU8>,
    bytes: [u8; negotiation::MAX_RECORD],
    len: usize,
    maximum: usize,
    last: u64,
    until: u64,
    send_by: u64,
    observation_until: Option<u64>,
}
impl fmt::Debug for Host {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HostStartup")
            .field("phase", &self.phase)
            .field("pending_bytes", &self.len)
            .finish_non_exhaustive()
    }
}
impl Host {
    /// The listener has established TLS/ALPN on a protected tailnet endpoint and
    /// obtained admission for those ACTUAL endpoints. This constructor verifies
    /// their equality; neither supplied addresses nor bootstrap bytes admit peers.
    pub fn from_admitted(
        native: NativeQuicUdpConnection,
        admission: Admission,
        config: Configuration,
    ) -> Result<Self, Error> {
        let cx = admission.context();
        Self::start(cx, native, Peer::Tailnet(admission), config)
    }
    fn start(
        cx: Cx,
        native: NativeQuicUdpConnection,
        peer: Peer,
        config: Configuration,
    ) -> Result<Self, Error> {
        let timeout = config.validate()?;
        let current = now(&cx)?;
        peer.check(&cx, Role::Observe)?;
        let until = current.checked_add(timeout).ok_or(Error::Clock)?;
        let (transport, routes) = QuicRecords::bootstrap(native, &cx, config.transport)?;
        peer.check_addresses(&transport)?;
        let authority = SessionAuthority::new(config.binding.remote_session, config.authority);
        let maximum =
            (config.offer.limits.max_control_message_bytes() as usize).min(negotiation::MAX_RECORD);
        Ok(Self {
            cx,
            peer: Some(peer),
            transport: Some(transport),
            routes,
            config,
            intersection: None,
            selected: None,
            authority: Some(authority),
            phase: Phase::Hello,
            role: Role::Observe,
            approval: Arc::new(AtomicU8::new(0)),
            bytes: [0; negotiation::MAX_RECORD],
            len: 0,
            maximum,
            last: current,
            until,
            send_by: until,
            observation_until: None,
        })
    }
    pub fn approval(&self) -> Option<Approval> {
        (self.phase == Phase::Approval && self.approval.load(Ordering::Acquire) == WAITING).then(
            || Approval {
                state: Arc::downgrade(&self.approval),
                cx: self.cx.clone(),
                deadline: self.until,
            },
        )
    }
    pub const fn deadline_us(&self) -> u64 {
        self.until
    }
    pub fn is_complete(&self) -> bool {
        self.phase == Phase::Complete
    }
    fn check(&mut self) -> Result<u64, Error> {
        if matches!(self.phase, Phase::Closed | Phase::Detached) {
            return Err(Error::Closed);
        }
        let current = now(&self.cx)?;
        if current < self.last {
            return Err(Error::Clock);
        }
        if current >= self.until || self.observation_until.is_some_and(|d| current >= d) {
            return Err(Error::Expired);
        }
        self.last = current;
        if matches!(self.approval.load(Ordering::Acquire), DENIED | RETIRED) {
            return Err(Error::Denied);
        }
        let peer = self.peer.as_ref().ok_or(Error::Closed)?;
        peer.check(&self.cx, self.role)?;
        let transport = self.transport.as_ref().ok_or(Error::Closed)?;
        peer.check_addresses(transport)?;
        if transport.receive_ended(self.routes.inbound)? {
            return Err(Error::Closed);
        }
        Ok(current)
    }
    fn permit(&self) -> Result<Permit<'_>, Error> {
        Ok(Permit {
            peer: self.peer.as_ref().ok_or(Error::Closed)?,
            cx: &self.cx,
            role: self.role,
            until: self
                .observation_until
                .map_or(self.until, |d| d.min(self.until)),
            decision: &self.approval,
        })
    }
    fn stage(&mut self, message: &Message) -> Result<(), Error> {
        if self.len != 0 {
            return Err(Error::Order);
        }
        let current = self.check()?;
        self.send_by = self
            .until
            .min(
                self.peer
                    .as_ref()
                    .ok_or(Error::Closed)?
                    .check(&self.cx, self.role)?,
            )
            .min(
                current
                    .checked_add(self.config.transport.record_lifetime_micros)
                    .ok_or(Error::Clock)?,
            );
        if let Some(until) = self.observation_until {
            self.send_by = self.send_by.min(until);
        }
        self.len = negotiation::encode(message, self.maximum, &mut self.bytes)?;
        Ok(())
    }
    fn open_observation(&mut self) -> Result<(), Error> {
        let current = self.check()?;
        if self.config.require_approval {
            self.approval
                .compare_exchange(ALLOWED, CONSUMED, Ordering::AcqRel, Ordering::Acquire)
                .map_err(|_| Error::Denied)?;
        }
        let authority = self.authority.as_mut().ok_or(Error::Closed)?;
        authority
            .authorize_observation(HostInstant::from_micros(current))
            .map_err(|_| Error::Authority)?;
        let until = authority
            .observation_deadline(HostInstant::from_micros(current))
            .map_err(|_| Error::Authority)?
            .as_micros();
        self.observation_until = Some(until);
        self.stage(&Message::SessionOpened {
            binding: self.config.binding,
            selection: self.selected.clone().ok_or(Error::Order)?,
            observation_until_us: until,
        })?;
        self.phase = Phase::Bind;
        Ok(())
    }
    fn consume(&mut self, bytes: &[u8]) -> Result<(), Error> {
        self.check()?;
        match (
            self.phase,
            negotiation::decode(bytes, self.maximum, self.routes.inbound.binding)?,
        ) {
            (Phase::Hello, Message::ClientHello(offer)) => {
                self.peer
                    .as_ref()
                    .ok_or(Error::Closed)?
                    .check(&self.cx, offer.role)?;
                let intersection = self.config.offer.intersect(&offer)?;
                self.maximum = (intersection.limits.max_control_message_bytes() as usize)
                    .min(negotiation::MAX_RECORD);
                self.role = offer.role;
                self.stage(&Message::HostCapabilities(intersection.clone()))?;
                self.intersection = Some(intersection);
                self.phase = Phase::Selection;
            }
            (Phase::Selection, Message::SelectedConfiguration(selection)) => {
                selection.check_against(self.intersection.as_ref().ok_or(Error::Order)?)?;
                self.maximum = (selection.limits.max_control_message_bytes() as usize)
                    .min(negotiation::MAX_RECORD);
                self.selected = Some(selection);
                self.authority
                    .as_mut()
                    .ok_or(Error::Closed)?
                    .mark_capabilities_checked()
                    .map_err(|_| Error::Authority)?;
                if self.config.require_approval {
                    self.authority
                        .as_mut()
                        .ok_or(Error::Closed)?
                        .require_approval()
                        .map_err(|_| Error::Authority)?;
                    self.stage(&Message::ApprovalRequired {
                        request: self.config.binding.remote_session,
                        deadline_us: self.until,
                        role: self.role,
                    })?;
                    self.approval.store(WAITING, Ordering::Release);
                    self.phase = Phase::Approval;
                } else {
                    self.open_observation()?;
                }
            }
            (Phase::Ack, Message::BindingAccepted { binding })
                if binding == self.config.binding.id =>
            {
                self.authority
                    .as_mut()
                    .ok_or(Error::Closed)?
                    .authorize_observation_delivery(HostInstant::from_micros(now(&self.cx)?))
                    .map_err(|_| Error::Authority)?;
                self.phase = Phase::Complete;
            }
            _ => return Err(Error::Order),
        }
        Ok(())
    }
    fn step(&mut self) -> Result<(), Error> {
        self.check()?;
        if self.phase == Phase::Complete {
            return Ok(());
        }
        if self.phase == Phase::Approval
            && self.len == 0
            && self.approval.load(Ordering::Acquire) == ALLOWED
        {
            self.open_observation()?;
        }
        // Take only for this synchronous call, never across an await.
        let mut transport = self.transport.take().ok_or(Error::Closed)?;
        let result: Result<Option<([u8; negotiation::MAX_RECORD], usize)>, Error> = (|| {
            let permit = self.permit()?;
            if self.len != 0 {
                match transport.send(
                    &self.cx,
                    Route::Stream(self.routes.outbound),
                    &self.bytes[..self.len],
                    self.send_by,
                    || permit.check(),
                ) {
                    Ok(()) => self.len = 0,
                    Err(quic::Error::Backpressure) => return Ok(None),
                    Err(e) => return Err(e.into()),
                }
            }
            if self.phase == Phase::Bind {
                let permit = self.permit()?;
                match transport.bind_control(
                    &self.cx,
                    self.routes,
                    self.config.binding.id,
                    self.maximum,
                    || permit.check(),
                ) {
                    Ok(routes) => {
                        self.routes = routes;
                        self.phase = Phase::Ack;
                    }
                    Err(quic::Error::Backpressure) => return Ok(None),
                    Err(e) => return Err(e.into()),
                }
            }
            let mut bytes = [0; negotiation::MAX_RECORD];
            let consumed = Cell::new(false);
            let mut len = 0;
            let permit = self.permit()?;
            transport.receive_ready(
                &self.cx,
                || permit.check(),
                |_| !consumed.get(),
                |route, record| {
                    if route != Route::Stream(self.routes.inbound) || record.len() > self.maximum {
                        return Err(());
                    }
                    bytes[..record.len()].copy_from_slice(record);
                    len = record.len();
                    consumed.set(true);
                    Ok(Disposition::Consumed)
                },
            )?;
            Ok(consumed.get().then_some((bytes, len)))
        })();
        self.transport = Some(transport);
        if let Some((bytes, len)) = result? {
            self.consume(&bytes[..len])?;
        }
        self.check()?;
        Ok(())
    }
    pub fn tick(&mut self) -> Result<(), Error> {
        let result = self.step();
        if result.is_err() {
            self.close();
        }
        result
    }
    /// Run one bounded reactor turn. Peer revalidation never refreshes the
    /// startup deadline, pending record deadline, or provisional observation.
    pub async fn drive(&mut self, wait: Duration) -> Result<(), Error> {
        let mut guard = Drive {
            host: self,
            completed: false,
        };
        let result = guard.host.drive_inner(wait).await;
        guard.completed = result.is_ok();
        result
    }
    async fn drive_inner(&mut self, wait: Duration) -> Result<(), Error> {
        if wait > Duration::from_millis(100) {
            return Err(Error::InvalidConfiguration);
        }
        self.step()?;
        if self.is_complete() {
            return Ok(());
        }
        let current = self.check()?;
        if self
            .peer
            .as_ref()
            .ok_or(Error::Closed)?
            .check(&self.cx, self.role)?
            .saturating_sub(current)
            < 250_000
        {
            self.peer.as_mut().ok_or(Error::Closed)?.refresh().await?;
            self.check()?;
        }
        let peer = self.peer.as_ref().ok_or(Error::Closed)?;
        let until = self
            .observation_until
            .map_or(self.until, |d| d.min(self.until));
        let permit = Permit {
            peer,
            cx: &self.cx,
            role: self.role,
            until,
            decision: &self.approval,
        };
        let wait = wait
            .min(Duration::from_micros(until.saturating_sub(now(&self.cx)?)))
            .min(Duration::from_micros(
                peer.check(&self.cx, self.role)?
                    .saturating_sub(now(&self.cx)?),
            ));
        self.transport
            .as_mut()
            .ok_or(Error::Closed)?
            .drive(&self.cx, wait, || permit.check())
            .await?;
        self.step()
    }
    /// Transfer only after receiving `BindingAccepted` on the bound stream. No
    /// input lease, decoder acknowledgement, or usable-view evidence is created.
    pub fn finish(mut self) -> Result<OpenedSession, Error> {
        self.check()?;
        if !self.is_complete() {
            return Err(Error::Order);
        }
        let authority = self.authority.take().ok_or(Error::Closed)?;
        let peer = self.peer.as_ref().ok_or(Error::Closed)?;
        let control = peer.observation(self.cx.clone(), authority)?;
        let transport = self.transport.take().ok_or(Error::Closed)?;
        let connection = transport.binding();
        let result = OpenedSession {
            cx: self.cx.clone(),
            peer: self.peer.take().ok_or(Error::Closed)?,
            transport,
            connection,
            routes: self.routes,
            control,
            binding: self.config.binding,
            selected: self.selected.take().ok_or(Error::Order)?,
            closed: false,
        };
        self.phase = Phase::Detached;
        self.approval.store(RETIRED, Ordering::Release);
        Ok(result)
    }
    pub fn close(&mut self) {
        if self.phase == Phase::Detached {
            return;
        }
        self.phase = Phase::Closed;
        self.approval.store(RETIRED, Ordering::Release);
        if let Some(authority) = &mut self.authority {
            authority.close();
        }
        if let Some(peer) = &self.peer {
            peer.revoke();
        }
        if let Some(transport) = &mut self.transport {
            transport.close();
        }
        self.bytes.fill(0);
        self.len = 0;
    }
}
impl Drop for Host {
    fn drop(&mut self) {
        self.close();
    }
}
struct Drive<'a> {
    host: &'a mut Host,
    completed: bool,
}
impl Drop for Drive<'_> {
    fn drop(&mut self) {
        if !self.completed {
            self.host.close();
        }
    }
}

/// Owns the negotiated connection and the actual observation lifetime together.
/// The enclosing broker still drives renewal, decoder startup, OS lifecycle and
/// (only after a usable view) input acquisition. Dropping this owner revokes its
/// shared observation before closing transport, even if a clone escaped.
pub struct OpenedSession {
    cx: Cx,
    peer: Peer,
    transport: QuicRecords,
    connection: ConnectionBinding,
    routes: ControlRoutes,
    control: ObservationControl,
    binding: ControlBinding,
    selected: Selection,
    closed: bool,
}
impl OpenedSession {
    pub fn check(&mut self) -> Result<(), Error> {
        let result = (|| {
            if self.closed || !self.transport.is_bound_to(&self.connection) {
                return Err(Error::Closed);
            }
            self.peer.check(&self.cx, self.selected.role)?;
            self.peer.check_addresses(&self.transport)?;
            if self.transport.receive_ended(self.routes.inbound)? {
                return Err(Error::Closed);
            }
            self.control.check().map_err(|_| Error::Authority)?;
            Ok(())
        })();
        if result.is_err() {
            self.close();
        }
        result
    }
    pub const fn binding(&self) -> ControlBinding {
        self.binding
    }
    pub fn selection(&self) -> &Selection {
        &self.selected
    }
    pub fn observation(&mut self) -> Result<ObservationControl, Error> {
        self.check()?;
        Ok(self.control.clone())
    }
    /// Borrow the same connection for the session's existing control/media/input
    /// dispatchers. Never replace it; the next check rejects a changed owner.
    pub fn io(&mut self) -> Result<(&mut QuicRecords, ControlRoutes), Error> {
        self.check()?;
        Ok((&mut self.transport, self.routes))
    }
    pub async fn refresh_admission(&mut self) -> Result<(), Error> {
        self.check()?;
        let mut guard = SessionRefresh {
            session: self,
            completed: false,
        };
        guard.session.peer.refresh().await?;
        guard.session.check()?;
        guard.completed = true;
        Ok(())
    }
    pub fn close(&mut self) {
        self.closed = true;
        self.control.revoke();
        self.peer.revoke();
        self.transport.close();
    }
}
impl Drop for OpenedSession {
    fn drop(&mut self) {
        self.close();
    }
}
struct SessionRefresh<'a> {
    session: &'a mut OpenedSession,
    completed: bool,
}
impl Drop for SessionRefresh<'_> {
    fn drop(&mut self) {
        if !self.completed {
            self.session.close();
        }
    }
}

#[cfg(test)]
mod tests;
