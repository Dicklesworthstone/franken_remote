//! Initial control grant over an existing authenticated connection. The broker
//! reserves the OS share-session's Seat before minting authority, initializes the
//! native owner outside the authority path, and publishes only after it is ready.
use super::{NegotiatedInput, QuicInput, Routes, ticket::TICKET_CADENCE_US};
use crate::{
    input_agent::{self, AdmissionGate, Driver, Phase, Seat},
    input_watchdog::StopReason,
    media::{Error as MediaError, ObservationControl, host_now},
};
use asupersync::{cx::Cx, net::quic_native::StreamRole};
use fr_core::{
    ids::{InputLeaseId, InputTicketId},
    input_submission::{InputMonitor, InputSink, PlatformError, Refusal},
    limits::ProtocolLimits,
};
use fr_transport::quic::{
    self, ConnectionBinding, ControlRoutes, Disposition, Messages, Priority, QuicRecords, Route,
};
use fr_wire::{
    Kind, WireError,
    control::{self, GRANTED_BYTES, Granted, Request, Target},
    input::{InputDelivery, InputDirection},
    negotiation::{ControlBinding, Role, Selection},
};
use std::time::Duration;

pub const CAPABILITY: &str = "native-control-grant";
const REQUEST_LIFETIME_US: u64 = 2_000_000;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    InvalidRoutes,
    InvalidWait,
    NotNegotiated,
    WrongSession,
    AlreadyAttached,
    WrongConnection,
    PeerClosed,
    RequestPending,
    NoRequest,
    Replay,
    Expired,
    Clock,
    Stopped,
    TargetChanged,
    CredentialsUnavailable,
    NativeNotReady,
    AlreadyGranted,
    Media(MediaError),
    Agent(input_agent::Error),
    Input(super::Error),
    Authority(Refusal),
    Wire(WireError),
    Transport(quic::Error),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Idle,
    AwaitingApproval,
    NativeStarting,
    Backpressure,
    /// Transport accepted the grant, not proof that the viewer received it.
    GrantQueued,
}
/// Supplied by the same authenticated session that installed the input routes.
/// A peer-provided tuple or a numeric channel ID is not an admission proof.
#[derive(Clone, Copy)]
pub struct Scope<'a> {
    pub parent: ControlBinding,
    pub control: ControlRoutes,
    pub selection: &'a Selection,
}
struct Waiting {
    request: Request,
    until: u64,
}
struct NativeGrant {
    input: QuicInput,
    monitor: InputMonitor,
    record: Granted,
    bytes: [u8; GRANTED_BYTES],
    until: u64,
    queued: bool,
}
/// One broker attachment per observation lifetime. A request cannot reserve the
/// Seat; explicit local approval is required. One successful native grant may be
/// handed off. Reacquisition uses a new explicit session, not a replayed request.
/// Poll the Driver returned by approve independently, including while native
/// initialization is blocked. No OS call or application callback holds an
/// authority mutex. No input/feedback route is installed implicitly.
pub struct GrantBroker {
    observation: ObservationControl,
    cx: Cx,
    admission: AdmissionGate,
    connection: ConnectionBinding,
    parent: ControlBinding,
    control_routes: ControlRoutes,
    input_routes: Routes,
    negotiated: Option<NegotiatedInput>,
    limits: ProtocolLimits,
    seat: Seat,
    floor: Option<u64>,
    waiting: Option<Waiting>,
    native: Option<NativeGrant>,
    last: u64,
    terminal: bool,
}
impl GrantBroker {
    /// Consume completed configuration/input negotiation on this same session.
    /// Keep its non-cloneable proof throughout approval, native initialization
    /// and grant publication. A request must name the exact negotiated display
    /// and view; local consent and native readiness remain independently required.
    pub fn from_negotiated(
        observation: ObservationControl,
        connection: &QuicRecords,
        seat: Seat,
        scope: Scope<'_>,
        input: NegotiatedInput,
    ) -> Result<Self, Error> {
        let routes = input
            .broker_routes(connection, scope.parent, scope.selection.limits)
            .map_err(Error::Input)?;
        let mut broker = Self::new(observation, connection, seat, scope, routes)?;
        broker.negotiated = Some(input);
        Ok(broker)
    }
    pub fn new(
        observation: ObservationControl,
        connection: &QuicRecords,
        seat: Seat,
        scope: Scope<'_>,
        input_routes: Routes,
    ) -> Result<Self, Error> {
        if scope.selection.validate().is_err()
            || scope.selection.role != Role::RequestControl
            || !scope
                .selection
                .capabilities
                .iter()
                .any(|c| c.name == CAPABILITY && c.version == 1)
        {
            return Err(Error::NotNegotiated);
        }
        let (cx, admission) = observation.input_origin().map_err(Error::Media)?;
        if let Some(lease) = &admission.tailnet {
            let endpoints = lease.addresses();
            if connection.addresses().map_err(Error::Transport)?
                != (endpoints.local, endpoints.peer)
            {
                return Err(Error::WrongConnection);
            }
        }
        let limits = scope.selection.limits;
        let parent = scope.parent;
        if parent.id == 0
            || parent.host_boot.as_raw() == 0
            || parent.os_session.as_raw() == 0
            || parent.remote_session.as_raw() == 0
            || !observation.belongs_to_session(parent.remote_session)
        {
            return Err(Error::WrongSession);
        }
        if connection.role() != Ok(StreamRole::Server)
            || scope.control.inbound.outbound
            || !scope.control.outbound.outbound
            || input_routes.results.messages != Messages::InputFeedback
            || input_routes.actions.binding == parent.id
            || [scope.control.inbound, scope.control.outbound]
                .iter()
                .any(|r| {
                    r.binding != parent.id
                        || r.messages != Messages::SessionControl
                        || r.priority != Priority::Critical
                        || r.maximum < GRANTED_BYTES
                        || r.maximum > limits.max_control_message_bytes() as usize
                        || !connection.has_route(Route::Stream(*r))
                })
            || !connection.has_route(Route::Stream(input_routes.actions))
            || !connection.has_route(Route::Stream(input_routes.results))
            || input_routes
                .pointer
                .is_some_and(|p| !connection.has_route(Route::Datagram(p)))
        {
            return Err(Error::InvalidRoutes);
        }
        if connection.is_closed()
            || connection
                .receive_ended(scope.control.inbound)
                .map_err(Error::Transport)?
            || connection
                .receive_ended(input_routes.actions)
                .map_err(Error::Transport)?
        {
            return Err(Error::PeerClosed);
        }
        let last = host_now(&cx).map_err(Error::Media)?.as_micros();
        if !observation.claim_control_grant() {
            return Err(Error::AlreadyAttached);
        }
        Ok(Self {
            observation,
            cx,
            admission,
            connection: connection.binding(),
            parent,
            control_routes: scope.control,
            input_routes,
            negotiated: None,
            limits,
            seat,
            floor: None,
            waiting: None,
            native: None,
            last,
            terminal: false,
        })
    }
    fn bound(&mut self, connection: &QuicRecords) -> Result<(), Error> {
        if !connection.is_bound_to(&self.connection) {
            self.stop();
            return Err(Error::WrongConnection);
        }
        Ok(())
    }
    fn live(&mut self, connection: &QuicRecords) -> Result<u64, Error> {
        if self.terminal {
            return Err(Error::Stopped);
        }
        if connection.is_closed()
            || connection
                .receive_ended(self.control_routes.inbound)
                .map_err(Error::Transport)?
            || connection
                .receive_ended(self.input_routes.actions)
                .map_err(Error::Transport)?
        {
            return Err(Error::PeerClosed);
        }
        if let Some(input) = &self.negotiated {
            input
                .broker_routes(connection, self.parent, self.limits)
                .map_err(Error::Input)?;
        }
        if !self.admission.permitted() {
            return Err(Error::Stopped);
        }
        if let Some(lease) = &self.admission.tailnet {
            let endpoints = lease.addresses();
            if connection.addresses().map_err(Error::Transport)?
                != (endpoints.local, endpoints.peer)
            {
                return Err(Error::WrongConnection);
            }
        }
        let now = self.observation.check().map_err(Error::Media)?.as_micros();
        if now < self.last {
            return Err(Error::Clock);
        }
        self.last = now;
        if self.waiting.as_ref().is_some_and(|p| now >= p.until) {
            return Err(Error::Expired);
        }
        if let Some(native) = &self.native {
            if now >= native.until {
                return Err(Error::Expired);
            }
            native
                .monitor
                .authorize_ticket(
                    native.record.ticket,
                    fr_core::time::HostInstant::from_micros(now),
                )
                .map_err(Error::Authority)?;
        }
        Ok(now)
    }
    /// Local cancellation fences input before closing this viewer's observation.
    /// Native cleanup/destruction, not this method, releases the global Seat.
    pub fn stop(&mut self) {
        self.terminal = true;
        self.waiting = None;
        if let Some(native) = &self.native {
            native.input.control().stop(StopReason::LocalRevoke);
        }
        self.observation.revoke();
    }
    /// Initialization and cleanup status; never native authority or credentials.
    pub fn native_status(&self) -> Option<input_agent::Status> {
        self.native.as_ref().map(|n| n.input.status())
    }
    /// The copied, validated target is a request for local approval, not consent.
    pub fn request(&self) -> Option<Request> {
        self.waiting.as_ref().map(|w| w.request)
    }
    /// A local refusal does not revoke viewing or consume another viewer's Seat.
    pub fn deny(&mut self) {
        if self.native.is_some() {
            self.stop();
        } else {
            self.waiting = None;
        }
    }
    /// Receive only this connection's control requests. Early input is terminal,
    /// including input queued while native initialization or grant sending stalls.
    /// Unrelated records remain with their current owner when it reports Blocked.
    pub fn receive(
        &mut self,
        connection: &mut QuicRecords,
        mut other: impl FnMut(Route, &[u8]) -> Result<Disposition, ()>,
    ) -> Result<usize, Error> {
        self.bound(connection)?;
        let mut operation = Operation {
            broker: self,
            connection,
            complete: false,
        };
        operation.broker.live(operation.connection)?;
        let cx = operation.broker.cx.clone();
        let observation = operation.broker.observation.clone();
        let mut failure = None;
        let n = operation.connection.receive(
            &cx,
            || observation.check().is_ok(),
            |route, bytes| {
                let kind = bytes.get(6..8);
                if route == Route::Stream(operation.broker.input_routes.actions)
                    || operation
                        .broker
                        .input_routes
                        .pointer
                        .is_some_and(|p| route == Route::Datagram(p))
                {
                    failure = Some(Error::NativeNotReady);
                    return Err(());
                }
                if route == Route::Stream(operation.broker.control_routes.inbound)
                    && kind == Some(&(Kind::ControlRequest as u16).to_be_bytes())
                {
                    let result = operation.broker.accept(bytes);
                    return match result {
                        Ok(()) => Ok(Disposition::Consumed),
                        Err(error) => {
                            failure = Some(error);
                            Err(())
                        }
                    };
                }
                other(route, bytes)
            },
        );
        if let Some(error) = failure {
            return Err(error);
        }
        let n = n.map_err(Error::Transport)?;
        operation.broker.live(operation.connection)?;
        operation.complete = true;
        Ok(n)
    }
    fn accept(&mut self, bytes: &[u8]) -> Result<(), Error> {
        if self.native.is_some() {
            return Err(Error::AlreadyGranted);
        }
        if self.waiting.is_some() {
            return Err(Error::RequestPending);
        }
        let request = control::decode_request(
            bytes,
            self.parent,
            &self.limits,
            InputDirection::ViewerToHost,
            InputDelivery::Reliable,
        )
        .map_err(Error::Wire)?;
        if self
            .negotiated
            .as_ref()
            .is_some_and(|input| !input.matches_request(request))
        {
            return Err(Error::TargetChanged);
        }
        if self.floor.is_some_and(|floor| request.sequence <= floor) {
            return Err(Error::Replay);
        }
        if request.target.display_binding == self.input_routes.actions.binding {
            return Err(Error::InvalidRoutes);
        }
        let now = self.observation.check().map_err(Error::Media)?.as_micros();
        let until = now.checked_add(REQUEST_LIFETIME_US).ok_or(Error::Clock)?;
        self.floor = Some(request.sequence);
        self.waiting = Some(Waiting { request, until });
        Ok(())
    }
    /// Call only from the locally authenticated approval owner, with its CURRENT
    /// exact target and readiness evidence. No remote message calls this method.
    /// The factory must construct the approved target/capabilities without input
    /// effects; it runs on the native thread. Revalidate native geometry there.
    /// Busy Seat returns without calling fresh, factory, or granting any lease.
    pub fn approve<S, F, C>(
        &mut self,
        connection: &QuicRecords,
        approved: Target,
        fresh: impl FnOnce() -> Option<(InputLeaseId, InputTicketId)>,
        factory: F,
        native_cleanup: C,
    ) -> Result<Driver, Error>
    where
        S: InputSink + 'static,
        F: FnOnce() -> Result<S, PlatformError> + Send + 'static,
        C: FnMut(&mut S) -> bool + Send + 'static,
    {
        self.bound(connection)?;
        if let Err(error) = self.live(connection) {
            self.stop();
            return Err(error);
        }
        let waiting = self.waiting.as_ref().ok_or(Error::NoRequest)?;
        if waiting.request.target != approved {
            return Err(Error::TargetChanged);
        }
        let reservation = self.seat.reserve().map_err(Error::Agent)?;
        let waiting = self.waiting.take().ok_or(Error::NoRequest)?;
        let (lease, ticket) = fresh()
            .filter(|(l, t)| l.as_raw() != 0 && t.as_raw() != 0)
            .ok_or(Error::CredentialsUnavailable)?;
        let now = self.live(connection)?;
        if now >= waiting.until {
            return Err(Error::Expired);
        }
        let (record, session) = self
            .observation
            .initial_input(
                waiting.request,
                self.input_routes.actions.binding,
                lease,
                ticket,
                &reservation,
            )
            .map_err(Error::Media)?;
        let mut provisional = Provisional {
            observation: self.observation.clone(),
            complete: false,
        };
        let monitor = session.monitor();
        let mut bytes = [0; GRANTED_BYTES];
        control::encode_granted(
            record,
            &mut bytes,
            &self.limits,
            InputDirection::HostToViewer,
            InputDelivery::Reliable,
        )
        .map_err(Error::Wire)?;
        let (agent, driver) = reservation
            .start(
                self.cx.clone(),
                session,
                input_agent::Route::new(record.input_channel, self.limits),
                factory,
                native_cleanup,
                self.admission.clone(),
            )
            .map_err(Error::Agent)?;
        let mut input = QuicInput::new(self.cx.clone(), agent, connection, self.input_routes)
            .map_err(Error::Input)?;
        // LeaseGranted already transports ticket sequence zero. Renewal must not
        // reuse it, call the credential source early, or restart its lifetime.
        input.ticket_sequence = Some(1);
        input.ticket_after_us = record
            .issued_at_us
            .checked_add(TICKET_CADENCE_US)
            .ok_or(Error::Clock)?;
        input.last_ticket = Some(record.ticket);
        self.native = Some(NativeGrant {
            input,
            monitor,
            record,
            bytes,
            until: waiting
                .until
                .min(record.ticket_until_us)
                .min(record.lease_until_us),
            queued: false,
        });
        provisional.complete = true;
        Ok(driver)
    }
    /// The caller supplies fresh qualified local view evidence on EVERY turn.
    /// No grant bytes leave while native initialization is pending. Real QUIC
    /// backpressure preserves the original record and exclusive native deadline.
    pub fn service(
        &mut self,
        connection: &mut QuicRecords,
        current: Option<Target>,
    ) -> Result<Event, Error> {
        self.bound(connection)?;
        let mut operation = Operation {
            broker: self,
            connection,
            complete: false,
        };
        operation.broker.live(operation.connection)?;
        if !operation.broker.native.as_ref().is_some_and(|n| n.queued) {
            operation.broker.reject_early_input(operation.connection)?;
        }
        let event = operation.broker.send(operation.connection, current)?;
        operation.broker.live(operation.connection)?;
        operation.complete = true;
        Ok(event)
    }
    fn reject_early_input(&self, connection: &mut QuicRecords) -> Result<(), Error> {
        let mut early = false;
        let result = connection.receive_ready(
            &self.cx,
            || self.observation.check().is_ok(),
            |route| self.input_routes.input(route).is_some(),
            |_, _| {
                early = true;
                Err(())
            },
        );
        if early {
            return Err(Error::NativeNotReady);
        }
        result.map_err(Error::Transport)?;
        Ok(())
    }
    fn send(
        &mut self,
        connection: &mut QuicRecords,
        current: Option<Target>,
    ) -> Result<Event, Error> {
        let Some(native) = &mut self.native else {
            return Ok(if self.waiting.is_some() {
                Event::AwaitingApproval
            } else {
                Event::Idle
            });
        };
        if current != Some(native.record.request.target) {
            return Err(Error::TargetChanged);
        }
        match native.input.status().phase {
            Phase::Starting => return Ok(Event::NativeStarting),
            Phase::Running => {}
            Phase::Cleaning | Phase::Finished => return Err(Error::NativeNotReady),
        }
        if native.queued {
            return Ok(Event::GrantQueued);
        }
        let result = connection.send(
            &self.cx,
            Route::Stream(self.control_routes.outbound),
            &native.bytes,
            native.until,
            || {
                self.admission.permitted()
                    && self.observation.check_control().is_ok()
                    && host_now(&self.cx).is_ok_and(|now| {
                        now.as_micros() < native.until
                            && native
                                .monitor
                                .authorize_ticket(native.record.ticket, now)
                                .is_ok()
                    })
            },
        );
        match result {
            Ok(()) => {
                native.queued = true;
                Ok(Event::GrantQueued)
            }
            Err(quic::Error::Backpressure) => Ok(Event::Backpressure),
            Err(error) => Err(Error::Transport(error)),
        }
    }
    /// Move the exact initialized owner, replay ledger and held-state machinery
    /// into normal input delivery. Calling before transport acceptance refuses
    /// and revokes the provisional grant; there is no replacement native owner.
    pub fn finish(
        mut self,
        connection: &QuicRecords,
        current: Option<Target>,
    ) -> Result<QuicInput, Error> {
        self.bound(connection)?;
        self.live(connection)?;
        let native = self.native.as_ref().ok_or(Error::NativeNotReady)?;
        if !native.queued || native.input.status().phase != Phase::Running {
            return Err(Error::NativeNotReady);
        }
        if current != Some(native.record.request.target) {
            return Err(Error::TargetChanged);
        }
        Ok(self.native.take().ok_or(Error::NativeNotReady)?.input)
    }
    /// Drive only this same connection while initialization/sending waits. Poll
    /// the native Driver independently. Even dropping an UNPOLLED future fences
    /// the provisional grant and closes this connection, never a replacement.
    pub fn drive<'a>(
        &'a mut self,
        connection: &'a mut QuicRecords,
        wait: Duration,
    ) -> impl Future<Output = Result<(), Error>> + 'a {
        let operation = Operation {
            broker: self,
            connection,
            complete: false,
        };
        async move {
            let mut operation = operation;
            operation.broker.bound(operation.connection)?;
            if wait > Duration::from_millis(100) {
                return Err(Error::InvalidWait);
            }
            operation.broker.live(operation.connection)?;
            let cx = operation.broker.cx.clone();
            let observation = operation.broker.observation.clone();
            operation
                .connection
                .drive(&cx, wait, || observation.check().is_ok())
                .await
                .map_err(Error::Transport)?;
            operation.broker.live(operation.connection)?;
            operation.complete = true;
            Ok(())
        }
    }
}
impl Drop for GrantBroker {
    fn drop(&mut self) {
        if self.native.is_some() {
            self.stop();
        }
    }
}
struct Provisional {
    observation: ObservationControl,
    complete: bool,
}
impl Drop for Provisional {
    fn drop(&mut self) {
        if !self.complete {
            self.observation.revoke();
        }
    }
}
struct Operation<'a> {
    broker: &'a mut GrantBroker,
    connection: &'a mut QuicRecords,
    complete: bool,
}
impl Drop for Operation<'_> {
    fn drop(&mut self) {
        if !self.complete {
            self.broker.stop();
            if self.connection.is_bound_to(&self.broker.connection) {
                self.connection.close();
            }
        }
    }
}
