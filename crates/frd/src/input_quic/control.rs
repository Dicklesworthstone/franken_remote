//! Controller renewal shares observation authority but never the native mailbox.
use super::QuicInput;
use crate::{
    input_agent::AdmissionGate,
    input_watchdog::{self, Control, StopReason},
    media::{Error as MediaError, ObservationControl},
};
use asupersync::{cx::Cx, net::quic_native::StreamRole};
use fr_core::{
    input_submission::{ControlLease, Refusal},
    limits::ProtocolLimits,
    time::HostInstant,
};
use fr_transport::quic::{
    ConnectionBinding, ControlRoutes, Disposition, Error as TransportError, Messages, Priority,
    QuicRecords, Route,
};
use fr_wire::{
    Kind, WireError,
    authority::{self, Binding, MAX_AUTHORITY_BYTES, Message, Scope},
    input::{InputDelivery, InputDirection},
};
use std::time::Duration;

const CADENCE_US: u64 = 1_000_000;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    InvalidRoutes,
    AuthorityMismatch,
    ForeignConnection,
    PeerClosed,
    NonceUnavailable,
    UnexpectedResponse,
    Clock,
    Expired,
    Stopped,
    Authority(Refusal),
    Media(MediaError),
    Wire(WireError),
    Transport(TransportError),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Idle,
    Backpressure,
    ChallengeQueued,
    AwaitingResponse,
}
struct Pending {
    nonce: u128,
    deadline: HostInstant,
    send_by: u64,
    queued: bool,
}
/// One non-replaceable attachment to the canonical native owner. Service on idle
/// turns; poll that owner's Driver independently. Neither network traffic nor a
/// ticket renews this lease. This capability never submits an OS operation.
/// Dropping it revokes input but does not claim native cleanup or close media.
pub struct ControlRenewal {
    lease: ControlLease,
    control: Control,
    observation: ObservationControl,
    admission: AdmissionGate,
    cx: Cx,
    connection: ConnectionBinding,
    routes: ControlRoutes,
    binding: Binding,
    limits: ProtocolLimits,
    next_issue: u64,
    last_nonce: Option<u128>,
    pending: Option<Pending>,
    bytes: [u8; MAX_AUTHORITY_BYTES],
    renewed_until: Option<HostInstant>,
}
impl QuicInput {
    /// The observation owner must be the SAME authority from which the native
    /// input session was attached, not a second grant with equal identifiers.
    /// Routes are the authenticated session's existing reliable control pair.
    pub fn control_renewal(
        &mut self,
        observation: ObservationControl,
        connection: &QuicRecords,
        routes: ControlRoutes,
    ) -> Result<ControlRenewal, Error> {
        if !connection.is_bound_to(&self.connection) {
            return Err(Error::ForeignConnection);
        }
        if connection.role() != Ok(StreamRole::Server)
            || routes.outbound.binding == 0
            || routes.outbound.binding != routes.inbound.binding
            || !routes.outbound.outbound
            || routes.inbound.outbound
            || [routes.outbound, routes.inbound].iter().any(|r| {
                r.messages != Messages::SessionControl
                    || r.priority != Priority::Critical
                    || r.maximum < MAX_AUTHORITY_BYTES
                    || r.maximum > self.limits.max_control_message_bytes() as usize
                    || !connection.has_route(Route::Stream(*r))
            })
        {
            return Err(Error::InvalidRoutes);
        }
        if connection
            .receive_ended(routes.inbound)
            .map_err(Error::Transport)?
        {
            return Err(Error::PeerClosed);
        }
        observation.check_control().map_err(Error::Media)?;
        let (mut lease, cx, admission) = self
            .agent
            .take_control_lease(&observation)
            .ok_or(Error::AuthorityMismatch)?;
        let now = input_watchdog::host_now(&cx).map_err(|_| Error::Clock)?;
        lease.deadline(now).map_err(Error::Authority)?;
        let binding = Binding {
            channel: routes.outbound.binding,
            session: lease.session(),
        };
        Ok(ControlRenewal {
            lease,
            control: self.agent.control(),
            observation,
            admission,
            cx,
            connection: connection.binding(),
            routes,
            binding,
            limits: self.limits,
            next_issue: now.as_micros(),
            last_nonce: None,
            pending: None,
            bytes: [0; MAX_AUTHORITY_BYTES],
            renewed_until: None,
        })
    }
}
impl ControlRenewal {
    pub fn stop(&mut self) {
        self.control.stop(StopReason::ClientDisconnected);
        self.lease.stop();
        self.pending = None;
        self.bytes.fill(0);
    }
    pub const fn renewed_until(&self) -> Option<HostInstant> {
        self.renewed_until
    }
    fn bound(&self, connection: &QuicRecords) -> Result<(), Error> {
        if !connection.is_bound_to(&self.connection) {
            self.control.stop(StopReason::ClientDisconnected);
            return Err(Error::ForeignConnection);
        }
        Ok(())
    }
    fn sample(&mut self) -> Result<HostInstant, Error> {
        if self.control.is_stopped() || self.cx.checkpoint().is_err() || !self.admission.permitted()
        {
            return Err(Error::Stopped);
        }
        self.observation.check_control().map_err(Error::Media)?;
        let now = input_watchdog::host_now(&self.cx).map_err(|_| Error::Clock)?;
        self.lease.deadline(now).map_err(Error::Authority)?;
        Ok(now)
    }
    fn live(&mut self, connection: &QuicRecords) -> Result<HostInstant, Error> {
        if !connection.has_route(Route::Stream(self.routes.inbound))
            || !connection.has_route(Route::Stream(self.routes.outbound))
        {
            return Err(Error::InvalidRoutes);
        }
        if connection
            .receive_ended(self.routes.inbound)
            .map_err(Error::Transport)?
        {
            return Err(Error::PeerClosed);
        }
        self.sample()
    }
    /// Nonces come only from the host's unpredictable, non-reusing source. Called
    /// once per due challenge, never while awaiting a response or backpressured.
    pub fn service(
        &mut self,
        connection: &mut QuicRecords,
        mut fresh_nonce: impl FnMut() -> Result<u128, ()>,
    ) -> Result<Event, Error> {
        self.bound(connection)?;
        let mut io = IoGuard::new(connection, self.control.clone());
        let now = self.live(io.connection)?;
        if self.pending.is_none() && now.as_micros() >= self.next_issue {
            let nonce = fresh_nonce().map_err(|()| Error::NonceUnavailable)?;
            if nonce == 0 || self.last_nonce == Some(nonce) {
                return Err(Error::NonceUnavailable);
            }
            let issued = self.sample()?;
            let old_deadline = self.lease.deadline(issued).map_err(Error::Authority)?;
            self.next_issue = issued
                .as_micros()
                .checked_add(CADENCE_US)
                .ok_or(Error::Clock)?;
            let deadline = self
                .lease
                .challenge(nonce, issued)
                .map_err(Error::Authority)?;
            authority::encode(
                Message::Challenge {
                    scope: Scope::Control(self.lease.lease()),
                    nonce,
                    deadline_micros: deadline.as_micros(),
                },
                self.binding,
                &self.limits,
                &mut self.bytes,
                InputDirection::HostToViewer,
                InputDelivery::Reliable,
            )
            .map_err(Error::Wire)?;
            self.pending = Some(Pending {
                nonce,
                deadline,
                send_by: self
                    .next_issue
                    .min(old_deadline.as_micros())
                    .min(deadline.as_micros()),
                queued: false,
            });
            self.last_nonce = Some(nonce);
        }
        let now = self.sample()?;
        let event = if let Some(pending) = &mut self.pending {
            if now >= pending.deadline || (!pending.queued && now.as_micros() >= pending.send_by) {
                return Err(Error::Expired);
            }
            if pending.queued {
                Event::AwaitingResponse
            } else {
                let control = self.control.clone();
                let observation = self.observation.clone();
                let admission = self.admission.clone();
                match io.connection.send(
                    &self.cx,
                    Route::Stream(self.routes.outbound),
                    &self.bytes,
                    pending.send_by,
                    || {
                        !control.is_stopped()
                            && admission.permitted()
                            && observation.check_control().is_ok()
                    },
                ) {
                    Ok(()) => {
                        pending.queued = true;
                        Event::ChallengeQueued
                    }
                    Err(TransportError::Backpressure) => Event::Backpressure,
                    Err(error) => return Err(Error::Transport(error)),
                }
            }
        } else {
            Event::Idle
        };
        self.live(io.connection)?;
        io.complete = true;
        Ok(event)
    }
    fn response(&mut self, nonce: u128) -> Result<(), Error> {
        if !self
            .pending
            .as_ref()
            .is_some_and(|p| p.queued && p.nonce == nonce)
        {
            return Err(Error::UnexpectedResponse);
        }
        let now = self.sample()?;
        self.renewed_until = Some(self.lease.respond(nonce, now).map_err(Error::Authority)?);
        self.pending = None;
        self.bytes.fill(0);
        Ok(())
    }
    /// Claim only control responses for this lease. Observation responses and
    /// unrelated channels remain unread when `other` returns Blocked.
    pub fn receive(
        &mut self,
        connection: &mut QuicRecords,
        mut other: impl FnMut(Route, &[u8]) -> Result<Disposition, ()>,
    ) -> Result<usize, Error> {
        self.bound(connection)?;
        let mut io = IoGuard::new(connection, self.control.clone());
        self.live(io.connection)?;
        let control = self.control.clone();
        let observation = self.observation.clone();
        let admission = self.admission.clone();
        let cx = self.cx.clone();
        let mut failure = None;
        let result = io.connection.receive(
            &cx,
            || {
                !control.is_stopped()
                    && admission.permitted()
                    && observation.check_control().is_ok()
            },
            |route, bytes| {
                let kind = bytes.get(6..8);
                if route == Route::Stream(self.routes.inbound)
                    && (kind == Some(&(Kind::Challenge as u16).to_be_bytes())
                        || kind == Some(&(Kind::ChallengeResponse as u16).to_be_bytes()))
                {
                    let decoded = authority::decode(
                        bytes,
                        self.binding,
                        &self.limits,
                        InputDirection::ViewerToHost,
                        InputDelivery::Reliable,
                    )
                    .map_err(Error::Wire);
                    let answer = match decoded {
                        Ok(Message::Response {
                            scope: Scope::Observation,
                            ..
                        }) => return other(route, bytes),
                        Ok(Message::Response {
                            scope: Scope::Control(lease),
                            nonce,
                        }) if lease == self.lease.lease() => self.response(nonce),
                        Ok(_) => Err(Error::UnexpectedResponse),
                        Err(error) => Err(error),
                    };
                    if let Err(error) = answer {
                        failure = Some(error);
                        return Err(());
                    }
                    return Ok(Disposition::Consumed);
                }
                other(route, bytes)
            },
        );
        if let Some(error) = failure {
            return Err(error);
        }
        let count = result.map_err(Error::Transport)?;
        self.live(io.connection)?;
        io.complete = true;
        Ok(count)
    }
    /// Guard ownership starts before polling. Abandoning I/O revokes native
    /// control; it cannot cancel or misroute bytes on a replacement connection.
    pub fn drive<'a>(
        &'a mut self,
        connection: &'a mut QuicRecords,
        wait: Duration,
    ) -> impl std::future::Future<Output = Result<(), Error>> + 'a {
        let bound = self.bound(connection);
        let guard = bound
            .is_ok()
            .then(|| IoGuard::new(connection, self.control.clone()));
        async move {
            bound?;
            let mut io = guard.expect("bound connection guard");
            let now = self.live(io.connection)?;
            let deadline = self.lease.deadline(now).map_err(Error::Authority)?;
            let remaining = Duration::from_micros(
                deadline
                    .as_micros()
                    .checked_sub(now.as_micros())
                    .ok_or(Error::Expired)?,
            );
            let control = self.control.clone();
            let observation = self.observation.clone();
            let admission = self.admission.clone();
            io.connection
                .drive(&self.cx, wait.min(remaining), || {
                    !control.is_stopped()
                        && admission.permitted()
                        && observation.check_control().is_ok()
                })
                .await
                .map_err(Error::Transport)?;
            self.live(io.connection)?;
            io.complete = true;
            Ok(())
        }
    }
}
impl Drop for ControlRenewal {
    fn drop(&mut self) {
        self.stop();
    }
}
struct IoGuard<'a> {
    connection: &'a mut QuicRecords,
    control: Control,
    complete: bool,
}
impl<'a> IoGuard<'a> {
    fn new(connection: &'a mut QuicRecords, control: Control) -> Self {
        Self {
            connection,
            control,
            complete: false,
        }
    }
}
impl Drop for IoGuard<'_> {
    fn drop(&mut self) {
        if !self.complete {
            self.control.stop(StopReason::ClientDisconnected);
            self.connection.close();
        }
    }
}
