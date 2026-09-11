//! Native session-clock exchange. All timestamps are sampled here from retained
//! runtime clocks; callers cannot substitute the sample or measured endpoints.
use super::{Error as MediaError, ObservationControl};
use crate::session_startup::OpenedSession;
use asupersync::{cx::Cx, net::quic_native::StreamRole, types::CancelKind};
use fr_client::{clock::ClockExchange, input::ClientInstant};
use fr_core::limits::ProtocolLimits;
use fr_media::freshness::{ClockCorrelation, ClockPolicy};
use fr_transport::quic::{
    ConnectionBinding, ControlRoutes, Disposition, Error as TransportError, QuicRecords, Route,
};
use fr_wire::{
    clock::{self, Message},
    input::{InputDelivery, InputDirection},
    negotiation::{ControlBinding, NATIVE_PROFILE, PROFILE_VERSION, Selection},
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

const MIN_INTERVAL_US: u64 = 100_000;
const REPLY_LIFETIME_US: u64 = 1_000_000;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Configuration,
    CapabilityMissing,
    ForeignConnection,
    PeerClosed,
    Stopped,
    Clock,
    Expired,
    Sequence,
    RateLimited,
    WrongRole,
    Media(MediaError),
    Transport(TransportError),
    Wire(fr_wire::WireError),
    Client(fr_client::clock::Error),
    Startup(crate::session_startup::Error),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Idle,
    AwaitingReply,
    Backpressure,
    ProbeQueued,
    ReplyQueued,
}
#[derive(Clone)]
struct Life {
    cx: Cx,
    control: Option<ObservationControl>,
    alive: Arc<AtomicBool>,
}
impl Life {
    fn now(&self) -> Result<u64, Error> {
        if !self.alive.load(Ordering::Acquire) || self.cx.checkpoint().is_err() {
            return Err(Error::Stopped);
        }
        if let Some(control) = &self.control {
            return control
                .check()
                .map(fr_core::time::HostInstant::as_micros)
                .map_err(Error::Media);
        }
        Ok(self.cx.timer_driver().ok_or(Error::Clock)?.now().as_nanos() / 1000)
    }
    fn stop(&self) {
        self.alive.store(false, Ordering::Release);
        if let Some(control) = &self.control {
            control.revoke();
        }
        self.cx.cancel_fast(CancelKind::User);
    }
}
struct PendingReply {
    bytes: [u8; clock::REPLY_BYTES],
    until: u64,
}
// At most one sub-kilobyte endpoint exists per connection. Keep its fixed
// storage inline instead of adding a heap allocation for an enum-size heuristic.
#[allow(clippy::large_enum_variant)]
enum Side {
    Host {
        pending: Option<PendingReply>,
        sequence: u64,
        next_request: u64,
    },
    Viewer {
        exchange: ClockExchange,
        next_request: u64,
        interval: u64,
    },
}
/// One negotiated, connection-owned clock endpoint. Service it even when idle.
/// This does not schedule a task, renew authority, enable input, or assert that
/// decoded pixels are visible. The enclosing session owns lifecycle and native
/// input cleanup; the supplied viewer Cx MUST be dedicated to that session.
pub struct ClockSync {
    life: Life,
    connection: ConnectionBinding,
    routes: ControlRoutes,
    binding: ControlBinding,
    limits: ProtocolLimits,
    last_now: u64,
    side: Side,
}
impl ClockSync {
    /// Production host entry: retain the actual startup binding, selected
    /// capability, admission/consent owner and its clock. No peer clock is trusted.
    pub fn from_opened(session: &mut OpenedSession) -> Result<Self, Error> {
        let control = session.observation().map_err(Error::Startup)?;
        let binding = session.binding();
        let selection = session.selection().clone();
        let (connection, routes) = session.io().map_err(Error::Startup)?;
        Self::host(control, connection, routes, binding, &selection)
    }
    /// For already-qualified compositions. `binding` must be the host's installed
    /// startup binding; plain numeric equality is not identity or OS consent.
    pub fn host(
        control: ObservationControl,
        connection: &mut QuicRecords,
        routes: ControlRoutes,
        binding: ControlBinding,
        selection: &Selection,
    ) -> Result<Self, Error> {
        if connection.role() != Ok(StreamRole::Server) {
            return Err(Error::WrongRole);
        }
        let session = control
            .authority
            .lock()
            .map_err(|_| Error::Media(MediaError::Poisoned))?
            .session();
        if binding.remote_session != session {
            return Err(Error::Configuration);
        }
        let life = Life {
            cx: control.cx.clone(),
            control: Some(control),
            alive: Arc::new(AtomicBool::new(true)),
        };
        let now = life.now()?;
        Self::attach(
            life,
            connection,
            routes,
            binding,
            selection,
            Side::Host {
                pending: None,
                sequence: 0,
                next_request: now,
            },
            now,
        )
    }
    /// The native client supplies the selection/binding accepted by its startup
    /// owner, authenticated server connection, and dedicated session context.
    pub fn viewer(
        cx: Cx,
        connection: &mut QuicRecords,
        routes: ControlRoutes,
        binding: ControlBinding,
        selection: &Selection,
        policy: ClockPolicy,
    ) -> Result<Self, Error> {
        if connection.role() != Ok(StreamRole::Client) {
            return Err(Error::WrongRole);
        }
        // A positive, bounded cadence prevents the shared control lane becoming
        // a timer flood. Refresh before the previous correlation expires.
        if policy.valid_for_us < 4 * MIN_INTERVAL_US {
            return Err(Error::Configuration);
        }
        let life = Life {
            cx,
            control: None,
            alive: Arc::new(AtomicBool::new(true)),
        };
        let now = life.now()?;
        let exchange = ClockExchange::new(binding, selection.limits, policy, ClientInstant(now))
            .map_err(Error::Client)?;
        let interval = (policy.valid_for_us / 2).min(1_000_000);
        Self::attach(
            life,
            connection,
            routes,
            binding,
            selection,
            Side::Viewer {
                exchange,
                next_request: now,
                interval,
            },
            now,
        )
    }
    fn attach(
        life: Life,
        connection: &mut QuicRecords,
        routes: ControlRoutes,
        binding: ControlBinding,
        selection: &Selection,
        side: Side,
        now: u64,
    ) -> Result<Self, Error> {
        if selection.version != 0
            || selection.profile != NATIVE_PROFILE
            || selection.profile_version != PROFILE_VERSION
            || selection
                .capabilities
                .iter()
                .filter(|c| c.name == clock::CAPABILITY && c.version == clock::VERSION)
                .count()
                != 1
        {
            return Err(Error::CapabilityMissing);
        }
        if binding.id == 0
            || binding.host_boot.as_raw() == 0
            || binding.os_session.as_raw() == 0
            || binding.remote_session.as_raw() == 0
            || routes.inbound.binding != binding.id
            || routes.outbound.binding != binding.id
            || [routes.inbound, routes.outbound]
                .iter()
                .any(|r| r.maximum > selection.limits.max_control_message_bytes() as usize)
        {
            return Err(Error::Configuration);
        }
        connection
            .tick(&life.cx, || life.now().is_ok())
            .map_err(Error::Transport)?;
        let identity = connection.claim_clock(routes).map_err(Error::Transport)?;
        Ok(Self {
            life,
            connection: identity,
            routes,
            binding,
            limits: selection.limits,
            last_now: now,
            side,
        })
    }
    /// Match the actual measured exchange to its original session before moving
    /// it into a persistent viewer. Equal clocks are not a substitute for this.
    pub fn matches_viewer(
        &self,
        connection: &QuicRecords,
        binding: ControlBinding,
        limits: ProtocolLimits,
    ) -> bool {
        matches!(self.side, Side::Viewer { .. })
            && connection.is_bound_to(&self.connection)
            && binding == self.binding
            && limits == self.limits
    }
    pub fn stop(&mut self) {
        self.life.stop();
        match &mut self.side {
            Side::Host { pending, .. } => *pending = None,
            Side::Viewer { exchange, .. } => exchange.stop(),
        }
    }
    fn bound(&self, connection: &QuicRecords) -> Result<(), Error> {
        if !connection.is_bound_to(&self.connection) {
            self.life.stop();
            return Err(Error::ForeignConnection);
        }
        Ok(())
    }
    fn live(&mut self, connection: &mut QuicRecords) -> Result<u64, Error> {
        connection
            .tick(&self.life.cx, || self.life.now().is_ok())
            .map_err(Error::Transport)?;
        if !connection.has_route(Route::Stream(self.routes.inbound))
            || !connection.has_route(Route::Stream(self.routes.outbound))
        {
            return Err(Error::Configuration);
        }
        if connection
            .receive_ended(self.routes.inbound)
            .map_err(Error::Transport)?
        {
            return Err(Error::PeerClosed);
        }
        let now = self.life.now()?;
        if now < self.last_now {
            return Err(Error::Clock);
        }
        self.last_now = now;
        match &mut self.side {
            Side::Host {
                pending: Some(pending),
                ..
            } if now >= pending.until => return Err(Error::Expired),
            Side::Viewer { exchange, .. } => {
                exchange.tick(ClientInstant(now)).map_err(Error::Client)?;
            }
            Side::Host { .. } => {}
        }
        Ok(now)
    }
    pub fn service(&mut self, connection: &mut QuicRecords) -> Result<Event, Error> {
        self.bound(connection)?;
        let mut guard = IoGuard::new(connection, self.life.clone());
        let now = self.live(guard.connection)?;
        let result = match &mut self.side {
            Side::Host { pending, .. } => {
                if let Some(reply) = pending {
                    match guard.connection.send(
                        &self.life.cx,
                        Route::Stream(self.routes.outbound),
                        &reply.bytes,
                        reply.until,
                        || self.life.now().is_ok(),
                    ) {
                        Ok(()) => {
                            *pending = None;
                            Event::ReplyQueued
                        }
                        Err(TransportError::Backpressure) => Event::Backpressure,
                        Err(e) => return Err(Error::Transport(e)),
                    }
                } else {
                    Event::Idle
                }
            }
            Side::Viewer {
                exchange,
                next_request,
                interval,
            } => {
                if !exchange.in_flight() && now >= *next_request {
                    exchange.begin(ClientInstant(now)).map_err(Error::Client)?;
                    *next_request = now.checked_add(*interval).ok_or(Error::Clock)?;
                }
                if let Some((bytes, until)) = exchange
                    .pending(ClientInstant(now))
                    .map_err(Error::Client)?
                {
                    match guard.connection.send(
                        &self.life.cx,
                        Route::Stream(self.routes.outbound),
                        bytes,
                        until,
                        || self.life.now().is_ok(),
                    ) {
                        Ok(()) => {
                            exchange
                                .queued(ClientInstant(self.life.now()?))
                                .map_err(Error::Client)?;
                            Event::ProbeQueued
                        }
                        Err(TransportError::Backpressure) => Event::Backpressure,
                        Err(e) => return Err(Error::Transport(e)),
                    }
                } else if exchange.in_flight() {
                    Event::AwaitingReply
                } else {
                    Event::Idle
                }
            }
        };
        self.live(guard.connection)?;
        guard.complete = true;
        Ok(result)
    }
    fn message(&mut self, bytes: &[u8]) -> Result<Disposition, Error> {
        let direction = if matches!(self.side, Side::Host { .. }) {
            InputDirection::ViewerToHost
        } else {
            InputDirection::HostToViewer
        };
        let message = clock::decode(
            bytes,
            self.binding,
            &self.limits,
            direction,
            InputDelivery::Reliable,
        )
        .map_err(Error::Wire)?;
        let now = self.life.now()?;
        if now < self.last_now {
            return Err(Error::Clock);
        }
        self.last_now = now;
        match (&mut self.side, message) {
            (
                Side::Host {
                    pending,
                    sequence,
                    next_request,
                },
                Message::Probe { sequence: incoming },
            ) => {
                if pending.is_some() {
                    return Ok(Disposition::Blocked);
                }
                if sequence.checked_add(1) != Some(incoming) {
                    return Err(Error::Sequence);
                }
                if now < *next_request {
                    return Err(Error::RateLimited);
                }
                let control = self.life.control.as_ref().ok_or(Error::Configuration)?;
                // Sample only after receiving/validating this request. Never
                // update the sample if a later transport admission is blocked.
                let sample = control.check().map_err(Error::Media)?.as_micros();
                if sample < now {
                    return Err(Error::Clock);
                }
                let until = control
                    .deadline(Duration::from_micros(REPLY_LIFETIME_US))
                    .map_err(Error::Media)?
                    .time()
                    .as_nanos()
                    / 1000;
                let until = until.min(sample.checked_add(REPLY_LIFETIME_US).ok_or(Error::Clock)?);
                let mut reply = PendingReply {
                    bytes: [0; clock::REPLY_BYTES],
                    until,
                };
                clock::encode(
                    Message::Reply {
                        sequence: incoming,
                        host_sample_us: sample,
                    },
                    self.binding,
                    &self.limits,
                    &mut reply.bytes,
                    InputDirection::HostToViewer,
                    InputDelivery::Reliable,
                )
                .map_err(Error::Wire)?;
                *next_request = sample.checked_add(MIN_INTERVAL_US).ok_or(Error::Clock)?;
                *sequence = incoming;
                *pending = Some(reply);
            }
            (
                Side::Viewer {
                    exchange,
                    next_request,
                    interval,
                },
                Message::Reply { .. },
            ) => {
                exchange
                    .accept(bytes, ClientInstant(now))
                    .map_err(Error::Client)?;
                *next_request = now.checked_add(*interval).ok_or(Error::Clock)?;
            }
            _ => return Err(Error::WrongRole),
        }
        Ok(Disposition::Consumed)
    }
    /// Unrelated control/media/input belongs to the enclosing session. Returning
    /// Blocked preserves its bytes for that dispatcher; it is never discarded.
    pub fn receive(
        &mut self,
        connection: &mut QuicRecords,
        mut other: impl FnMut(Route, &[u8]) -> Result<Disposition, ()>,
    ) -> Result<usize, Error> {
        self.bound(connection)?;
        let mut guard = IoGuard::new(connection, self.life.clone());
        self.live(guard.connection)?;
        let life = self.life.clone();
        let mut failure = None;
        let count = guard.connection.receive(
            &life.cx,
            || life.now().is_ok(),
            |route, bytes| {
                let kind = bytes.get(6..8);
                if kind == Some(&(fr_wire::Kind::ClockProbe as u16).to_be_bytes())
                    || kind == Some(&(fr_wire::Kind::ClockReply as u16).to_be_bytes())
                {
                    if route != Route::Stream(self.routes.inbound) {
                        failure = Some(Error::Configuration);
                        return Err(());
                    }
                    return self.message(bytes).map_err(|e| {
                        failure = Some(e);
                    });
                }
                other(route, bytes)
            },
        );
        if let Some(e) = failure {
            return Err(e);
        }
        let count = count.map_err(Error::Transport)?;
        self.live(guard.connection)?;
        guard.complete = true;
        Ok(count)
    }
    /// The last usable measured sample, not proof of live source or visibility.
    /// This checks the same connection and context each time. Copied historical
    /// correlations never supersede the receiver's own lifecycle guards.
    pub fn correlation(
        &mut self,
        connection: &mut QuicRecords,
    ) -> Result<Option<ClockCorrelation>, Error> {
        self.bound(connection)?;
        let mut guard = IoGuard::new(connection, self.life.clone());
        let now = self.live(guard.connection)?;
        let Side::Viewer { exchange, .. } = &mut self.side else {
            return Err(Error::WrongRole);
        };
        let result = exchange
            .correlation(ClientInstant(now))
            .map_err(Error::Client)?;
        guard.complete = true;
        Ok(result)
    }
    /// Construct the failure guard before returning a future, including a future
    /// that is dropped unpolled. Do not drive one connection concurrently.
    pub fn drive<'a>(
        &'a mut self,
        connection: &'a mut QuicRecords,
        wait: Duration,
    ) -> impl std::future::Future<Output = Result<(), Error>> + 'a {
        let bound = self.bound(connection);
        let guard = bound
            .is_ok()
            .then(|| IoGuard::new(connection, self.life.clone()));
        async move {
            bound?;
            let mut guard = guard.expect("matching connection installs guard");
            self.live(guard.connection)?;
            let life = self.life.clone();
            guard
                .connection
                .drive(&life.cx, wait, || life.now().is_ok())
                .await
                .map_err(Error::Transport)?;
            self.live(guard.connection)?;
            guard.complete = true;
            Ok(())
        }
    }
}
impl Drop for ClockSync {
    fn drop(&mut self) {
        self.stop();
    }
}
struct IoGuard<'a> {
    connection: &'a mut QuicRecords,
    life: Life,
    complete: bool,
}
impl<'a> IoGuard<'a> {
    fn new(connection: &'a mut QuicRecords, life: Life) -> Self {
        Self {
            connection,
            life,
            complete: false,
        }
    }
}
impl Drop for IoGuard<'_> {
    fn drop(&mut self) {
        if !self.complete {
            self.life.stop();
            self.connection.close();
        }
    }
}
