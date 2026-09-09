//! Observation renewal on the existing bound session-control streams. This
//! attaches to the approved media owner; it cannot grant input or refresh pixels.
use super::{Error as MediaError, ObservationControl};
use asupersync::net::quic_native::StreamRole;
use fr_core::{limits::ProtocolLimits, time::HostInstant};
use fr_transport::quic::{
    ConnectionBinding, ControlRoutes, Disposition, Error as TransportError, Messages, Priority,
    QuicRecords, Route,
};
use fr_wire::{
    Kind, WireError,
    authority::{self, Binding, Message, OBSERVATION_CHALLENGE_BYTES, Scope},
    input::{InputDelivery, InputDirection},
};
use std::{sync::atomic::Ordering, time::Duration};

const CADENCE_MICROS: u64 = 1_000_000;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    InvalidRoutes,
    AlreadyAttached,
    ForeignConnection,
    PeerClosed,
    NonceUnavailable,
    UnexpectedResponse,
    Clock,
    Expired,
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
/// One renewal attachment for the lifetime of an `ObservationControl`, including
/// all its clones. Poll bounded turns during idle. The existing final media
/// checks/worker deadlines remain independently authoritative if polling stalls.
/// Dropping this owner ends observation and shared admission, not OS rollback.
pub struct ObservationRenewal {
    control: ObservationControl,
    connection: ConnectionBinding,
    routes: ControlRoutes,
    binding: Binding,
    limits: ProtocolLimits,
    next_issue: u64,
    last_nonce: Option<u128>,
    pending: Option<Pending>,
    bytes: [u8; OBSERVATION_CHALLENGE_BYTES],
    renewed_until: Option<HostInstant>,
}
impl ObservationRenewal {
    /// Routes come from the already admitted/approved startup owner. Numeric
    /// bindings alone are NOT a Tailscale admission or local consent proof.
    pub fn new(
        control: ObservationControl,
        connection: &QuicRecords,
        routes: ControlRoutes,
        limits: ProtocolLimits,
    ) -> Result<Self, Error> {
        if connection.role() != Ok(StreamRole::Server)
            || routes.outbound.binding == 0
            || routes.outbound.binding != routes.inbound.binding
            || !routes.outbound.outbound
            || routes.inbound.outbound
            || [routes.outbound, routes.inbound].iter().any(|route| {
                route.messages != Messages::SessionControl
                    || route.priority != Priority::Critical
                    || route.maximum < OBSERVATION_CHALLENGE_BYTES
                    || route.maximum > limits.max_control_message_bytes() as usize
                    || !connection.has_route(Route::Stream(*route))
            })
        {
            return Err(Error::InvalidRoutes);
        }
        let now = control.check().map_err(Error::Media)?;
        let session = control
            .authority
            .lock()
            .map_err(|_| Error::Media(MediaError::Poisoned))?
            .session();
        if session.as_raw() == 0 {
            return Err(Error::InvalidRoutes);
        }
        if connection
            .receive_ended(routes.inbound)
            .map_err(Error::Transport)?
        {
            return Err(Error::PeerClosed);
        }
        // This is sticky: after drop this authority is terminal, not available
        // for a replacement renewer carrying old challenges or borrowed routes.
        control
            .renewal_attached
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| Error::AlreadyAttached)?;
        Ok(Self {
            control,
            connection: connection.binding(),
            routes,
            binding: Binding {
                channel: routes.outbound.binding,
                session,
            },
            limits,
            next_issue: now.as_micros(),
            last_nonce: None,
            pending: None,
            bytes: [0; OBSERVATION_CHALLENGE_BYTES],
            renewed_until: None,
        })
    }
    pub fn stop(&mut self) {
        self.control.revoke();
        self.pending = None;
        self.bytes.fill(0);
    }
    /// The last successful host deadline, not a newly computed receipt-time TTL.
    pub const fn renewed_until(&self) -> Option<HostInstant> {
        self.renewed_until
    }
    fn bound(&self, connection: &QuicRecords) -> Result<(), Error> {
        if !connection.is_bound_to(&self.connection) {
            self.control.revoke();
            return Err(Error::ForeignConnection);
        }
        Ok(())
    }
    fn live(&self, connection: &QuicRecords) -> Result<HostInstant, Error> {
        let now = self.control.check().map_err(Error::Media)?;
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
        Ok(now)
    }
    /// `fresh_nonce` is the host's qualified unpredictable, non-reusing nonce
    /// source. It is called only when no challenge is outstanding and cadence is
    /// due. Never pass a nonce received from the viewer or derive one from time.
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
            let issued = self.control.check().map_err(Error::Media)?;
            self.next_issue = issued
                .as_micros()
                .checked_add(CADENCE_MICROS)
                .ok_or(Error::Clock)?;
            let deadline = self.control.issue_challenge(nonce).map_err(Error::Media)?;
            let send_by = self
                .control
                .deadline(Duration::from_secs(1))
                .map_err(Error::Media)?
                .time()
                .as_nanos()
                / 1000;
            authority::encode(
                Message::Challenge {
                    scope: Scope::Observation,
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
            self.last_nonce = Some(nonce);
            self.pending = Some(Pending {
                nonce,
                deadline,
                send_by: send_by.min(deadline.as_micros()),
                queued: false,
            });
        }
        let event = if let Some(pending) = &mut self.pending {
            let now = self.control.check().map_err(Error::Media)?;
            if now >= pending.deadline || (!pending.queued && now.as_micros() >= pending.send_by) {
                return Err(Error::Expired);
            }
            if pending.queued {
                Event::AwaitingResponse
            } else {
                match io.connection.send(
                    &self.control.cx,
                    Route::Stream(self.routes.outbound),
                    &self.bytes,
                    pending.send_by,
                    || self.control.check().is_ok(),
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
            .is_some_and(|pending| pending.queued && pending.nonce == nonce)
        {
            return Err(Error::UnexpectedResponse);
        }
        self.renewed_until = Some(self.control.renew(nonce).map_err(Error::Media)?);
        self.pending = None;
        self.bytes.fill(0);
        Ok(())
    }
    /// Consume observation responses only. Other control/media/input records
    /// stay with the enclosing session's bounded handler. In particular a control
    /// response is not silently interpreted as an observation renewal.
    pub fn receive(
        &mut self,
        connection: &mut QuicRecords,
        mut other: impl FnMut(Route, &[u8]) -> Result<Disposition, ()>,
    ) -> Result<usize, Error> {
        self.bound(connection)?;
        let mut io = IoGuard::new(connection, self.control.clone());
        self.live(io.connection)?;
        let control = self.control.clone();
        let mut failure = None;
        let result = io.connection.receive(
            &control.cx,
            || control.check().is_ok(),
            |route, bytes| {
                let kind = bytes.get(6..8);
                if route == Route::Stream(self.routes.inbound)
                    && (kind == Some(&(Kind::Challenge as u16).to_be_bytes())
                        || kind == Some(&(Kind::ChallengeResponse as u16).to_be_bytes()))
                {
                    let message = authority::decode(
                        bytes,
                        self.binding,
                        &self.limits,
                        InputDirection::ViewerToHost,
                        InputDelivery::Reliable,
                    )
                    .map_err(Error::Wire);
                    match message {
                        Ok(Message::Response {
                            scope: Scope::Observation,
                            nonce,
                        }) => {
                            if let Err(error) = self.response(nonce) {
                                failure = Some(error);
                                return Err(());
                            }
                            return Ok(Disposition::Consumed);
                        }
                        Ok(_) => {}
                        Err(error) => {
                            failure = Some(error);
                            return Err(());
                        }
                    }
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
    /// Synchronous guard construction also fences a dropped unpolled future.
    /// Connection replacement cannot redirect this owner or close its successor.
    pub fn drive<'a>(
        &'a self,
        connection: &'a mut QuicRecords,
        wait: Duration,
    ) -> impl std::future::Future<Output = Result<(), Error>> + 'a {
        let bound = self.bound(connection);
        let guard = bound
            .is_ok()
            .then(|| IoGuard::new(connection, self.control.clone()));
        async move {
            bound?;
            let mut io = guard.expect("matching connection installs guard");
            self.live(io.connection)?;
            io.connection
                .drive(&self.control.cx, wait, || self.control.check().is_ok())
                .await
                .map_err(Error::Transport)?;
            self.live(io.connection)?;
            io.complete = true;
            Ok(())
        }
    }
}
impl Drop for ObservationRenewal {
    fn drop(&mut self) {
        self.stop();
    }
}
struct IoGuard<'a> {
    connection: &'a mut QuicRecords,
    control: ObservationControl,
    complete: bool,
}
impl<'a> IoGuard<'a> {
    fn new(connection: &'a mut QuicRecords, control: ObservationControl) -> Self {
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
            self.control.revoke();
            self.connection.close();
        }
    }
}
