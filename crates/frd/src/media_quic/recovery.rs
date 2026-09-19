//! Reliable failure reports for the original negotiated receiver and connection.
//! This owner does not replace channels, claim presentation, or grant input.
use super::{Error as MediaError, NegotiatedMedia};
use asupersync::{cx::Cx, net::quic_native::StreamRole};
use fr_core::limits::ProtocolLimits;
use fr_media::delivery::{
    DecodedFrame, DeliveryError, MediaEpoch, ReceivePipeline, RecoveryOffer, RecoveryRequestor,
};
use fr_transport::quic::{
    self, ConnectionBinding, ControlRoutes, Messages, Priority, QuicRecords, Route,
};
use fr_wire::{decoder::Binding, negotiation::ControlBinding, recovery_request::REQUEST_BYTES};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Media(MediaError),
    Delivery(DeliveryError),
    Transport(quic::Error),
    NotNegotiated,
    WrongBinding,
    Closed,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Error {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Receiving,
    /// The receiver is fenced; the unchanged request is waiting for send credit.
    Pending,
    /// Transport admission only, NOT host receipt or successful recovery.
    Requested,
    Closed,
}

/// Retains one fixed record and its original generation/deadline through actual
/// QUIC backpressure. Keep this owner alongside the receiver, even after sending:
/// waiting for a replacement still has an absolute timeout. Dropping it never
/// closes some other connection. Its parent session owns lifetime cancellation.
pub struct Receiver {
    connection: ConnectionBinding,
    routes: ControlRoutes,
    requestor: RecoveryRequestor,
    bytes: [u8; REQUEST_BYTES],
    pending: Option<RecoveryOffer>,
    state: State,
}
impl std::fmt::Debug for Receiver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecoveryReceiver")
            .field("state", &self.state)
            .field("deadline", &self.next_deadline())
            .finish_non_exhaustive()
    }
}

pub(super) fn control_binding(
    q: &QuicRecords,
    routes: ControlRoutes,
    parent: ControlBinding,
    mut view: Binding,
    limits: ProtocolLimits,
) -> Result<Binding, Error> {
    if q.is_closed() {
        return Err(Error::Closed);
    }
    if parent.host_boot != view.parent.host_boot
        || parent.os_session != view.parent.os_session
        || parent.remote_session != view.parent.remote_session
        || routes.inbound.outbound
        || !routes.outbound.outbound
        || [routes.inbound, routes.outbound].iter().any(|r| {
            r.binding != parent.id
                || r.messages != Messages::SessionControl
                || r.priority != Priority::Critical
                || r.maximum < REQUEST_BYTES
                || !q.has_route(Route::Stream(*r))
        })
        || u64::from(limits.max_control_message_bytes())
            < u64::try_from(REQUEST_BYTES).map_err(|_| Error::WrongBinding)?
        || q.receive_ended(routes.inbound).map_err(Error::Transport)?
    {
        return Err(Error::WrongBinding);
    }
    view.parent = parent;
    view.validate().map_err(|_| Error::WrongBinding)?;
    Ok(view)
}

impl NegotiatedMedia {
    /// Use only after positive reference-recovery negotiation. The same completed
    /// media attachments and receiver configuration are checked before creating
    /// a failure-report owner; copied numbers cannot substitute for connection
    /// ownership. The caller still admits replacement channels independently.
    pub fn recovery_receiver(
        &self,
        q: &QuicRecords,
        routes: ControlRoutes,
        parent: ControlBinding,
        receiver: &ReceivePipeline,
    ) -> Result<Receiver, Error> {
        self.check(q).map_err(Error::Media)?;
        self.check_recovery_capability()
            .map_err(|_| Error::NotNegotiated)?;
        if q.role().map_err(Error::Transport)? != StreamRole::Client {
            return Err(Error::WrongBinding);
        }
        let view = self.binding();
        receiver
            .check_delivery_configuration(
                self.limits(),
                self.bindings(),
                MediaEpoch {
                    configuration: view.configuration,
                    recovery: view.recovery,
                },
            )
            .map_err(Error::Delivery)?;
        let binding = control_binding(q, routes, parent, view, *self.limits().protocol())?;
        Ok(Receiver {
            connection: q.binding(),
            routes,
            requestor: RecoveryRequestor::new(receiver, binding).map_err(Error::Delivery)?,
            bytes: [0; REQUEST_BYTES],
            pending: None,
            state: State::Receiving,
        })
    }
}
impl Receiver {
    pub const fn state(&self) -> State {
        self.state
    }
    pub fn next_deadline(&self) -> Option<u64> {
        self.requestor.next_deadline()
    }
    /// Feed the original successful completion before presentation consumes it.
    /// No successful decode is inferred from receipt of a picture or heartbeat.
    pub fn observe_decoded(&mut self, frame: &DecodedFrame) -> Result<(), Error> {
        self.requestor
            .observe_decoded(frame)
            .map_err(Error::Delivery)
    }
    /// Service even during silence. This method ticks the actual receiver before
    /// encoding or sending, so queued pictures/input evidence are already fenced.
    /// On success, unrelated control/renewal records remain with the running
    /// session. A terminal error never refreshes the original recovery attempt.
    pub fn service(
        &mut self,
        cx: &Cx,
        q: &mut QuicRecords,
        receiver: &mut ReceivePipeline,
        authorize: impl FnMut() -> bool,
    ) -> Result<State, Error> {
        let result = self.service_inner(cx, q, receiver, authorize);
        if result.is_err() {
            self.close();
        }
        result
    }
    fn service_inner(
        &mut self,
        cx: &Cx,
        q: &mut QuicRecords,
        receiver: &mut ReceivePipeline,
        mut authorize: impl FnMut() -> bool,
    ) -> Result<State, Error> {
        if self.state == State::Closed {
            return Err(Error::Closed);
        }
        // Do not tick, close, or mutate a different transport/receiver first.
        if !q.is_bound_to(&self.connection) {
            return Err(Error::WrongBinding);
        }
        if q.is_closed()
            || !q.has_route(Route::Stream(self.routes.outbound))
            || q.receive_ended(self.routes.inbound)
                .map_err(Error::Transport)?
        {
            return Err(Error::Closed);
        }
        cx.checkpoint()
            .map_err(|_| Error::Transport(quic::Error::Cancelled))?;
        if !authorize() {
            return Err(Error::Transport(quic::Error::Unauthorized));
        }
        let now = quic_now(cx)?;
        if self.pending.is_none() {
            self.pending = self
                .requestor
                .offer(receiver, now, &mut self.bytes)
                .map_err(Error::Delivery)?;
        }
        let Some(offer) = &self.pending else {
            return Ok(self.state);
        };
        self.state = State::Pending;
        self.requestor
            .authorize_write(receiver, offer, now)
            .map_err(Error::Delivery)?;
        let mut refusal = None;
        let sent = q.send(
            cx,
            Route::Stream(self.routes.outbound),
            &self.bytes[..offer.byte_len()],
            offer.send_by_micros(),
            || {
                let valid = (|| {
                    if !authorize() {
                        return Err(Error::Transport(quic::Error::Unauthorized));
                    }
                    self.requestor
                        .authorize_write(receiver, offer, quic_now(cx)?)
                        .map_err(Error::Delivery)
                })();
                if let Err(error) = valid {
                    refusal = Some(error);
                    false
                } else {
                    true
                }
            },
        );
        if let Some(error) = refusal {
            return Err(error);
        }
        match sent {
            Ok(()) => {
                self.requestor
                    .mark_sent(receiver, offer, quic_now(cx)?)
                    .map_err(Error::Delivery)?;
                self.pending = None;
                self.bytes.fill(0);
                self.state = State::Requested;
            }
            Err(quic::Error::Backpressure) => {}
            Err(error) => return Err(Error::Transport(error)),
        }
        Ok(self.state)
    }
    pub fn close(&mut self) {
        self.requestor.close();
        self.pending = None;
        self.bytes.fill(0);
        self.state = State::Closed;
    }
}
fn quic_now(cx: &Cx) -> Result<u64, Error> {
    Ok(cx
        .timer_driver()
        .ok_or(Error::Transport(quic::Error::Clock))?
        .now()
        .as_nanos()
        / 1000)
}
