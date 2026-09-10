//! One-use native media-configuration attachment on an established control pair.
//! Both directions are actual allocated QUIC streams. No app record is routed
//! until the binding and attachment exchanges complete on this connection.
use super::{
    ConnectionBinding, ControlRoutes, Cx, Disposition, EmptySendState, Error, Inbound, MAX_STREAMS,
    Messages, Priority, QuicRecords, RecordStream, Route, Sender, StreamId, StreamRole,
    StreamRoute,
};
use asupersync::bytes::Bytes;
use fr_core::limits::ProtocolLimits;
use fr_wire::{
    attachment::{self, Descriptor, GRANT_RECORD_BYTES, Grant, MediaRole, Message, Ticket},
    decoder::Binding,
    input::{InputDelivery, InputDirection},
    negotiation::{ControlBinding, Selection},
};
use std::{
    cell::Cell,
    fmt,
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
    time::Duration,
};

const PENDING: u8 = 0;
const ARMED: u8 = 1;
const ACTIVE: u8 = 2;
const ABANDONED: u8 = 3;
const MAX_WAIT_US: u64 = 2_000_000;

/// Retained through connection closure: consumed bindings and ticket identities
/// never re-enter the allocation pool. At most seven auxiliary pairs exist.
pub(super) struct Reservation {
    state: Arc<AtomicU8>,
    until: u64,
    ticket: Option<Ticket>,
    pub(super) inbound: StreamId,
}
impl Reservation {
    pub(super) fn readable(&self) -> bool {
        self.state.load(Ordering::Acquire) != PENDING
    }
    pub(super) fn expired(&self, now: u64) -> bool {
        match self.state.load(Ordering::Acquire) {
            ACTIVE => false,
            PENDING | ARMED => now >= self.until,
            _ => true,
        }
    }
}
/// The application's completed negotiation, not a source of authentication.
/// Obtain these values from the same running host/viewer session.
#[derive(Clone, Copy)]
pub struct ChannelScope<'a> {
    pub control: ControlRoutes,
    pub parent: ControlBinding,
    pub selection: &'a Selection,
}
/// Locally authorized view and host-owned unpredictable ticket. The helper does
/// not choose displays, mint session IDs, or expand observation permission.
#[derive(Clone, Copy)]
pub struct ChannelRequest {
    pub binding: Binding,
    pub ticket: Ticket,
    pub timeout: Duration,
}
/// Newly installed configuration/reply routes. They remain on the original
/// connection and retain native flow-control and old-send reservations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttachedChannel {
    pub descriptor: Descriptor,
    pub outbound: StreamRoute,
    pub inbound: StreamRoute,
    pub byte_allowance: u64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Offer,
    BindingAck,
    Ticket,
    Attach,
    Attached,
    Promote,
    Complete,
    Closed,
}
/// Non-cloneable exchange owner. Abandonment fences the connection on its next
/// checked operation, including idle ticks; no detached task or timer is needed.
pub struct MediaChannel {
    connection: ConnectionBinding,
    state: Arc<AtomicU8>,
    host: bool,
    parent: ControlBinding,
    limits: ProtocolLimits,
    control: ControlRoutes,
    pair: ControlRoutes,
    descriptor: Descriptor,
    grant: Option<Grant>,
    phase: Phase,
    pending: [u8; GRANT_RECORD_BYTES],
    len: usize,
    last: u64,
    until: u64,
    result: Option<AttachedChannel>,
    receive_armed: bool,
}
impl fmt::Debug for MediaChannel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MediaChannel")
            .field("phase", &self.phase)
            .field("pending_bytes", &self.len)
            .finish_non_exhaustive()
    }
}
impl Drop for MediaChannel {
    fn drop(&mut self) {
        if self.phase != Phase::Complete {
            self.state.store(ABANDONED, Ordering::Release);
        }
        self.pending.fill(0);
    }
}
fn validate_scope(q: &QuicRecords, scope: &ChannelScope<'_>) -> Result<(), Error> {
    scope
        .selection
        .validate()
        .map_err(|_| Error::InvalidPolicy)?;
    if !scope
        .selection
        .capabilities
        .iter()
        .any(|c| c.name == attachment::CAPABILITY && c.version == attachment::VERSION)
        || scope.parent.id == 0
        || scope.parent.host_boot.as_raw() == 0
        || scope.parent.os_session.as_raw() == 0
        || scope.parent.remote_session.as_raw() == 0
        || scope.control.inbound.outbound
        || !scope.control.outbound.outbound
        || [scope.control.inbound, scope.control.outbound]
            .iter()
            .any(|r| {
                r.messages != Messages::SessionControl
                    || r.binding != scope.parent.id
                    || r.priority != Priority::Critical
                    || r.maximum < GRANT_RECORD_BYTES
                    || !q.has_route(Route::Stream(*r))
            })
        || (scope.selection.limits.max_control_message_bytes() as usize) < GRANT_RECORD_BYTES
        || q.receive_ended(scope.control.inbound)?
    {
        return Err(Error::WrongRoute);
    }
    Ok(())
}
fn duration_us(duration: Duration) -> Result<u64, Error> {
    let us = u64::try_from(duration.as_micros()).map_err(|_| Error::InvalidPolicy)?;
    if us == 0 || us > MAX_WAIT_US {
        return Err(Error::InvalidPolicy);
    }
    Ok(us)
}
impl QuicRecords {
    fn next_uni(&self, role: StreamRole) -> Result<StreamId, Error> {
        let n = self
            .streams
            .iter()
            .filter(|r| r.stream.is_local_for(role))
            .map(|r| r.stream.0)
            .max()
            .ok_or(Error::WrongRoute)?;
        let next = n
            .checked_add(4)
            .filter(|n| *n < (1_u64 << 62))
            .ok_or(Error::WrongRoute)?;
        Ok(StreamId(next))
    }
    fn validate_reservation(
        &self,
        d: Descriptor,
        ticket: Option<Ticket>,
        allowance: u64,
    ) -> Result<(StreamRole, u64, u64), Error> {
        if self.streams.len() + 2 > MAX_STREAMS
            || self.attachments.len() >= (MAX_STREAMS - 2) / 2
            || self
                .attachments
                .iter()
                .any(|r| matches!(r.state.load(Ordering::Acquire), PENDING | ARMED))
        {
            return Err(Error::Backpressure);
        }
        if d.binding.parent.id
            <= self
                .streams
                .iter()
                .map(|r| r.binding)
                .chain(self.datagrams.iter().map(|r| r.binding))
                .max()
                .unwrap_or(0)
            || ticket.is_some_and(|t| self.attachments.iter().any(|r| r.ticket == Some(t)))
            || d.role != MediaRole::Configuration
            || allowance < GRANT_RECORD_BYTES as u64
            || allowance > self.policy.stream_window
            || allowance > self.policy.critical_send_bytes as u64
        {
            return Err(Error::WrongRoute);
        }
        let role = self.role()?;
        let (local, peer) = if role == StreamRole::Server {
            (d.host_stream, d.viewer_stream)
        } else {
            (d.viewer_stream, d.host_stream)
        };
        let peer_role = if role == StreamRole::Server {
            StreamRole::Client
        } else {
            StreamRole::Server
        };
        if StreamId(local) != self.next_uni(role)? || StreamId(peer) != self.next_uni(peer_role)? {
            return Err(Error::WrongRoute);
        }
        Ok((role, local, peer))
    }
    fn reserve_pair(
        &mut self,
        cx: &Cx,
        d: Descriptor,
        until: u64,
        ticket: Option<Ticket>,
        allowance: u64,
    ) -> Result<(ControlRoutes, Arc<AtomicU8>), Error> {
        let (role, local, peer) = self.validate_reservation(d, ticket, allowance)?;
        self.streams
            .try_reserve_exact(2)
            .map_err(|_| Error::Allocation)?;
        self.inbound
            .try_reserve_exact(1)
            .map_err(|_| Error::Allocation)?;
        self.senders
            .try_reserve_exact(1)
            .map_err(|_| Error::Allocation)?;
        self.attachments
            .try_reserve_exact(1)
            .map_err(|_| Error::Allocation)?;
        let outbound = StreamRoute {
            stream: StreamId(local),
            binding: d.binding.parent.id,
            messages: Messages::Exact(if role == StreamRole::Server {
                0x1a
            } else {
                0x19
            }),
            priority: Priority::Critical,
            outbound: true,
            maximum: GRANT_RECORD_BYTES,
        };
        let inbound = StreamRoute {
            stream: StreamId(peer),
            outbound: false,
            messages: Messages::Exact(if role == StreamRole::Server {
                0x19
            } else {
                0x1a
            }),
            ..outbound
        };
        let framing = RecordStream::new(
            GRANT_RECORD_BYTES,
            inbound.binding,
            self.policy.record_lifetime_micros,
        )?;
        let result = (|| {
            let n = self.native.as_mut().ok_or(Error::Closed)?;
            if n.connection_mut()
                .open_uni_stream(cx)
                .map_err(|_| Error::Native)?
                != outbound.stream
            {
                return Err(Error::WrongRoute);
            }
            let empty = n
                .connection()
                .inner()
                .streams()
                .stream(outbound.stream)
                .map_err(|_| Error::Native)?
                .clone();
            // A server must not send MAX_STREAM_DATA for a client stream that
            // the client has not opened yet. BindingAccepted is that barrier.
            if role == StreamRole::Client {
                let actual = n
                    .connection_mut()
                    .configure_stream_receive_window(cx, inbound.stream, allowance)
                    .map_err(|_| Error::Native)?;
                if actual > allowance {
                    return Err(Error::InvalidPolicy);
                }
            }
            Ok(empty)
        })();
        let empty = match result {
            Ok(empty) => empty,
            Err(e) => {
                self.close();
                return Err(e);
            }
        };
        let state = Arc::new(AtomicU8::new(if role == StreamRole::Client {
            ARMED
        } else {
            PENDING
        }));
        self.streams.extend([outbound, inbound]);
        self.senders.push(Sender {
            route: outbound,
            empty: EmptySendState(empty),
            bytes: 0,
            records: 0,
            until: None,
        });
        self.inbound.push(Inbound {
            route: inbound,
            framing,
            remainder: Bytes::new(),
            fin: false,
        });
        self.attachments.push(Reservation {
            state: state.clone(),
            until,
            ticket,
            inbound: inbound.stream,
        });
        Ok((ControlRoutes { outbound, inbound }, state))
    }
    /// Reserve actual native routes before exposing a binding or issuing a
    /// ticket. A second pending exchange refuses instead of growing a queue.
    pub fn offer_media_channel(
        &mut self,
        cx: &Cx,
        scope: ChannelScope<'_>,
        request: ChannelRequest,
        mut authorize: impl FnMut() -> bool,
    ) -> Result<MediaChannel, Error> {
        let current = self.check(cx, &mut authorize)?;
        validate_scope(self, &scope)?;
        if self.role()? != StreamRole::Server || request.ticket.0 == 0 {
            return Err(Error::WrongRoute);
        }
        let until = current
            .checked_add(duration_us(request.timeout)?)
            .ok_or(Error::Clock)?;
        let descriptor = Descriptor {
            binding: request.binding,
            role: MediaRole::Configuration,
            host_stream: self.next_uni(StreamRole::Server)?.0,
            viewer_stream: self.next_uni(StreamRole::Client)?.0,
        };
        descriptor
            .validate(scope.parent)
            .map_err(|_| Error::WrongRoute)?;
        let allowance = u64::from(scope.selection.limits.max_control_message_bytes())
            .min(self.policy.stream_window)
            .min(self.policy.critical_send_bytes as u64);
        let grant = Grant {
            descriptor,
            ticket: request.ticket,
            deadline_us: until,
            byte_allowance: allowance,
            picture_allowance: 0,
            credit_epoch: u64::from(descriptor.binding.parent.id),
        };
        let (pair, state) =
            self.reserve_pair(cx, descriptor, until, Some(request.ticket), allowance)?;
        let mut owner = MediaChannel::new(
            self.binding(),
            state,
            scope,
            pair,
            descriptor,
            current,
            until,
            true,
        );
        owner.grant = Some(grant);
        owner.stage(Message::Binding(descriptor))?;
        owner.check(self, cx, &mut authorize)?;
        Ok(owner)
    }
    /// Install a received descriptor only from the session's authenticated,
    /// bound control dispatcher. Unnegotiated roles and foreign parents refuse.
    pub fn accept_media_channel(
        &mut self,
        cx: &Cx,
        scope: ChannelScope<'_>,
        bytes: &[u8],
        timeout: Duration,
        mut authorize: impl FnMut() -> bool,
    ) -> Result<MediaChannel, Error> {
        let current = self.check(cx, &mut authorize)?;
        validate_scope(self, &scope)?;
        if self.role()? != StreamRole::Client {
            return Err(Error::WrongRoute);
        }
        let Message::Binding(d) = attachment::decode(
            bytes,
            scope.parent,
            scope.parent.id,
            &scope.selection.limits,
            InputDirection::HostToViewer,
            InputDelivery::Reliable,
        )
        .map_err(|_| Error::Malformed)?
        else {
            return Err(Error::WrongRoute);
        };
        let until = current
            .checked_add(duration_us(timeout)?)
            .ok_or(Error::Clock)?;
        let allowance = u64::from(scope.selection.limits.max_control_message_bytes())
            .min(self.policy.stream_window)
            .min(self.policy.critical_send_bytes as u64);
        let (pair, state) = self.reserve_pair(cx, d, until, None, allowance)?;
        let mut owner =
            MediaChannel::new(self.binding(), state, scope, pair, d, current, until, false);
        owner.stage(Message::Accepted(d.binding.parent.id))?;
        owner.check(self, cx, &mut authorize)?;
        Ok(owner)
    }
}
impl MediaChannel {
    #[allow(clippy::too_many_arguments)]
    fn new(
        connection: ConnectionBinding,
        state: Arc<AtomicU8>,
        scope: ChannelScope<'_>,
        pair: ControlRoutes,
        descriptor: Descriptor,
        last: u64,
        until: u64,
        host: bool,
    ) -> Self {
        Self {
            connection,
            state,
            host,
            parent: scope.parent,
            limits: scope.selection.limits,
            control: scope.control,
            pair,
            descriptor,
            grant: None,
            phase: Phase::Offer,
            pending: [0; GRANT_RECORD_BYTES],
            len: 0,
            last,
            until,
            result: None,
            receive_armed: !host,
        }
    }
    pub const fn deadline_us(&self) -> u64 {
        self.until
    }
    pub const fn descriptor(&self) -> Descriptor {
        self.descriptor
    }
    pub fn is_complete(&self) -> bool {
        self.phase == Phase::Complete
    }
    pub fn close(&mut self) {
        self.state.store(ABANDONED, Ordering::Release);
        self.phase = Phase::Closed;
        self.pending.fill(0);
        self.len = 0;
        self.grant = None;
        self.result = None;
    }
    fn check(
        &mut self,
        q: &mut QuicRecords,
        cx: &Cx,
        authorize: &mut impl FnMut() -> bool,
    ) -> Result<u64, Error> {
        let result = (|| {
            let n = q.check(cx, authorize)?;
            if !q.is_bound_to(&self.connection) || self.phase == Phase::Closed {
                return Err(Error::WrongRoute);
            }
            if n < self.last {
                return Err(Error::Clock);
            }
            if n >= self.until && self.phase != Phase::Complete {
                return Err(Error::Expired);
            }
            if q.receive_ended(self.control.inbound)?
                || (self.receive_armed && q.receive_ended(self.pair.inbound)?)
            {
                return Err(Error::Closed);
            }
            self.last = n;
            Ok(n)
        })();
        if result.is_err() {
            self.close();
            q.close();
        }
        result
    }
    fn stage(&mut self, m: Message) -> Result<(), Error> {
        if self.len != 0 {
            return Err(Error::WrongRoute);
        }
        self.len = attachment::encode(
            m,
            self.parent,
            &self.limits,
            &mut self.pending,
            if self.host {
                InputDirection::HostToViewer
            } else {
                InputDirection::ViewerToHost
            },
            InputDelivery::Reliable,
        )
        .map_err(|_| Error::Malformed)?;
        Ok(())
    }
    /// An exact record enters transport ownership once. Repeated calls under
    /// backpressure do not rewrite its bytes, ticket, reservation or deadline.
    pub fn transmit(
        &mut self,
        q: &mut QuicRecords,
        cx: &Cx,
        mut authorize: impl FnMut() -> bool,
    ) -> Result<bool, Error> {
        self.check(q, cx, &mut authorize)?;
        if self.host && self.phase == Phase::Ticket && !self.receive_armed {
            let result = (|| {
                let grant = self.grant.ok_or(Error::WrongRoute)?;
                let actual = q
                    .native
                    .as_mut()
                    .ok_or(Error::Closed)?
                    .connection_mut()
                    .configure_stream_receive_window(
                        cx,
                        self.pair.inbound.stream,
                        grant.byte_allowance,
                    )
                    .map_err(|_| Error::Native)?;
                if actual > grant.byte_allowance {
                    return Err(Error::InvalidPolicy);
                }
                Ok(())
            })();
            if let Err(e) = result {
                self.close();
                q.close();
                return Err(e);
            }
            self.receive_armed = true;
            self.state.store(ARMED, Ordering::Release);
        }
        if self.len == 0 {
            return Ok(false);
        }
        let route = if matches!(self.phase, Phase::Offer | Phase::Ticket) {
            self.control.outbound
        } else {
            self.pair.outbound
        };
        match q.send(
            cx,
            Route::Stream(route),
            &self.pending[..self.len],
            self.until,
            &mut authorize,
        ) {
            Ok(()) => {
                self.pending.fill(0);
                self.len = 0;
                self.phase = match (self.host, self.phase) {
                    (true, Phase::Offer) => Phase::BindingAck,
                    (true, Phase::Ticket) => Phase::Attach,
                    (true, Phase::Attached) => Phase::Promote,
                    (false, Phase::Offer) => Phase::Ticket,
                    (false, Phase::Attach) => Phase::Attached,
                    _ => {
                        self.close();
                        q.close();
                        return Err(Error::WrongRoute);
                    }
                };
                Ok(true)
            }
            Err(Error::Backpressure) => Ok(false),
            Err(e) => {
                self.close();
                q.close();
                Err(e)
            }
        }
    }
    /// Dispatch a single attachment record, preserving every unrelated lane for
    /// its existing session owner. Callback processing does not run native media.
    pub fn dispatch(
        &mut self,
        q: &mut QuicRecords,
        cx: &Cx,
        mut authorize: impl FnMut() -> bool,
    ) -> Result<(), Error> {
        self.check(q, cx, &mut authorize)?;
        let incoming = match (self.host, self.phase) {
            (true, Phase::BindingAck) | (false, Phase::Ticket) => self.control.inbound,
            (true, Phase::Attach) | (false, Phase::Attached) => self.pair.inbound,
            _ => return Ok(()),
        };
        let ready = Cell::new(true);
        let mut failure = None;
        let result = q.receive_ready(
            cx,
            &mut authorize,
            |r| ready.get() && r == Route::Stream(incoming),
            |_, bytes| {
                ready.set(false);
                // Observation renewal and other control messages retain their
                // own dispatcher. Never consume them as attachment traffic.
                let kind = u16::from_be_bytes([bytes[6], bytes[7]]);
                if incoming == self.control.inbound && !matches!(kind, 0x0018..=0x001c) {
                    return Ok(Disposition::Blocked);
                }
                let r = self.received(bytes, incoming.binding);
                match r {
                    Ok(()) => Ok(Disposition::Consumed),
                    Err(e) => {
                        failure = Some(e);
                        Err(())
                    }
                }
            },
        );
        if let Some(e) = failure {
            self.close();
            q.close();
            return Err(e);
        }
        if let Err(e) = result {
            self.close();
            q.close();
            return Err(e);
        }
        self.check(q, cx, &mut authorize)?;
        Ok(())
    }
    fn received(&mut self, bytes: &[u8], binding: u32) -> Result<(), Error> {
        let m = attachment::decode(
            bytes,
            self.parent,
            binding,
            &self.limits,
            if self.host {
                InputDirection::ViewerToHost
            } else {
                InputDirection::HostToViewer
            },
            InputDelivery::Reliable,
        )
        .map_err(|_| Error::Malformed)?;
        match (self.host, self.phase, m) {
            (true, Phase::BindingAck, Message::Accepted(id))
                if id == self.descriptor.binding.parent.id =>
            {
                self.stage(Message::Ticket(self.grant.ok_or(Error::WrongRoute)?))?;
                self.phase = Phase::Ticket;
            }
            (true, Phase::Attach, Message::Attach(g)) if Some(g) == self.grant => {
                // The unique mutable owner consumes the ticket before staging
                // any acknowledgement. Even identical repeated Attach refuses.
                self.stage(Message::Attached(g))?;
                self.phase = Phase::Attached;
            }
            (false, Phase::Ticket, Message::Ticket(g)) if g.descriptor == self.descriptor => {
                // Host time is deliberately not compared to viewer time.
                if g.byte_allowance < GRANT_RECORD_BYTES as u64 {
                    return Err(Error::TooLarge);
                }
                self.grant = Some(g);
                self.stage(Message::Attach(g))?;
                self.phase = Phase::Attach;
            }
            (false, Phase::Attached, Message::Attached(g)) if Some(g) == self.grant => {
                self.phase = Phase::Promote;
            }
            _ => return Err(Error::WrongRoute),
        }
        Ok(())
    }
    /// Complete in place, after the ACK's old stream bytes are staged. Retained
    /// native retransmission bytes keep their original deadlines and charges.
    pub fn finish(
        &mut self,
        q: &mut QuicRecords,
        cx: &Cx,
        mut authorize: impl FnMut() -> bool,
    ) -> Result<Option<AttachedChannel>, Error> {
        self.check(q, cx, &mut authorize)?;
        if self.phase == Phase::Complete {
            return Ok(self.result);
        }
        if self.phase != Phase::Promote {
            return Ok(None);
        }
        if !q.send_staged(self.pair.outbound)? {
            return Ok(None);
        }
        let result = (|| {
            let slot = q
                .inbound
                .iter()
                .position(|r| r.route == self.pair.inbound)
                .ok_or(Error::WrongRoute)?;
            let receiving = &q.inbound[slot];
            if receiving.fin
                || receiving.framing.buffered_bytes() != 0
                || !receiving.remainder.is_empty()
            {
                return Err(Error::WrongRoute);
            }
            if self.host {
                let s = q
                    .native
                    .as_ref()
                    .ok_or(Error::Closed)?
                    .connection()
                    .inner()
                    .streams()
                    .stream(self.pair.inbound.stream)
                    .map_err(|_| Error::Native)?;
                if s.recv_offset != s.read_offset {
                    return Err(Error::WrongRoute);
                }
            }
            let grant = self.grant.ok_or(Error::WrongRoute)?;
            let maximum = usize::try_from(grant.byte_allowance).map_err(|_| Error::TooLarge)?;
            if maximum > q.policy.critical_send_bytes
                || grant.byte_allowance > q.policy.stream_window
            {
                return Err(Error::TooLarge);
            }
            let outbound = StreamRoute {
                messages: if self.host {
                    Messages::Exact(0x30)
                } else {
                    Messages::DecoderReplies
                },
                maximum,
                ..self.pair.outbound
            };
            let inbound = StreamRoute {
                messages: if self.host {
                    Messages::DecoderReplies
                } else {
                    Messages::Exact(0x30)
                },
                maximum,
                ..self.pair.inbound
            };
            let framing =
                RecordStream::new(maximum, inbound.binding, q.policy.record_lifetime_micros)?;
            let sender = q
                .senders
                .iter()
                .position(|r| r.route == self.pair.outbound)
                .ok_or(Error::WrongRoute)?;
            q.check(cx, &mut authorize)?;
            for r in &mut q.streams {
                if *r == self.pair.outbound {
                    *r = outbound;
                } else if *r == self.pair.inbound {
                    *r = inbound;
                }
            }
            q.inbound[slot].route = inbound;
            q.inbound[slot].framing = framing;
            q.senders[sender].route = outbound;
            self.pair = ControlRoutes { outbound, inbound };
            self.state.store(ACTIVE, Ordering::Release);
            self.phase = Phase::Complete;
            self.result = Some(AttachedChannel {
                descriptor: self.descriptor,
                outbound,
                inbound,
                byte_allowance: grant.byte_allowance,
            });
            self.grant = None;
            Ok(self.result)
        })();
        if result.is_err() {
            self.close();
            q.close();
        }
        result
    }
}
