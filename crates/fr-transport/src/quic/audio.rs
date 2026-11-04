//! The independently selected microphone pair on its original QUIC connection.
//! Route ownership is not a controller grant, host approval or local talk consent.
use super::{
    AttachedChannel, ConnectionBinding, Cx, DatagramReceipt, Disposition, Error, MediaChannel,
    Messages, Priority, QuicRecords, Route, SendAdmission,
};
use fr_core::{audio::AudioDirection, limits::ProtocolLimits};
use fr_wire::{attachment::MediaRole, audio, negotiation::ControlBinding};
use std::cell::Cell;

pub use super::attachment::RetirementRequest;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Host,
    Controller,
}

/// Non-cloneable owner of one completed `AudioUp` attachment. Configuration and
/// stop records use its reliable pair; only the controller sends packet datagrams.
/// Every operation checks object identity, including after optional retirement.
pub struct AudioUpChannel {
    attachment: MediaChannel,
    connection: ConnectionBinding,
    routes: AttachedChannel,
    parent: ControlBinding,
    limits: ProtocolLimits,
    side: Side,
    maximum: usize,
    packet_maximum: usize,
}

impl std::fmt::Debug for AudioUpChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AudioUpChannel([original microphone attachment])")
    }
}
impl Drop for AudioUpChannel {
    fn drop(&mut self) {
        self.attachment.close();
    }
}
impl AudioUpChannel {
    /// Consume the actual completed exchange, including its original negotiated
    /// limits. The session separately binds this proof to its original input owner
    /// and checks host approval, local talk/OS permission and native readiness.
    pub fn new(q: &QuicRecords, mut attachment: MediaChannel) -> Result<Self, Error> {
        let setup = (|| {
            let routes = attachment.completed_on(q)?;
            let parent = attachment.completed_parent(q)?;
            let limits = attachment.completed_limits(q)?;
            let packet = routes.datagram.ok_or(Error::WrongRoute)?;
            let (side, outbound, inbound) = if attachment.host {
                (Side::Host, Messages::AudioReplies, Messages::AudioControl)
            } else {
                (
                    Side::Controller,
                    Messages::AudioControl,
                    Messages::AudioReplies,
                )
            };
            if routes.descriptor.role != MediaRole::AudioUp
                || routes.outbound.messages != outbound
                || routes.inbound.messages != inbound
                || routes.outbound.priority != Priority::Critical
                || routes.inbound.priority != Priority::Critical
                || packet.kind != 0x0062
                || packet.outbound == attachment.host
                || packet.binding != routes.descriptor.binding.parent.id
            {
                return Err(Error::WrongRoute);
            }
            let maximum = usize::try_from(routes.byte_allowance).map_err(|_| Error::TooLarge)?;
            Ok((
                routes,
                parent,
                limits,
                side,
                maximum,
                q.datagram_maximum(packet),
            ))
        })();
        match setup {
            Ok((routes, parent, limits, side, maximum, packet_maximum)) => {
                let channel = Self {
                    attachment,
                    connection: q.binding(),
                    routes,
                    parent,
                    limits,
                    side,
                    maximum,
                    packet_maximum,
                };
                channel.check(q)?;
                Ok(channel)
            }
            Err(error) => {
                attachment.close();
                Err(error)
            }
        }
    }
    pub const fn side(&self) -> Side {
        self.side
    }
    pub fn is_host(&self) -> bool {
        self.side == Side::Host
    }
    pub const fn parent(&self) -> ControlBinding {
        self.parent
    }
    pub const fn binding(&self) -> u32 {
        self.routes.descriptor.binding.parent.id
    }
    pub const fn channel(&self) -> u32 {
        self.binding()
    }
    /// Exact limits from this connection's completed selection, without defaults
    /// or a caller-supplied reconstruction that could enlarge another ceiling.
    pub const fn limits(&self) -> ProtocolLimits {
        self.limits
    }
    pub const fn maximum_record_bytes(&self) -> usize {
        self.maximum
    }
    pub const fn packet_maximum(&self) -> usize {
        self.packet_maximum
    }
    pub fn owns_inbound(&self, route: Route) -> bool {
        route == Route::Stream(self.routes.inbound)
            || self
                .routes
                .datagram
                .is_some_and(|packet| !packet.outbound && route == Route::Datagram(packet))
    }
    fn original(&self, q: &QuicRecords) -> Result<(), Error> {
        if q.is_bound_to(&self.connection) {
            Ok(())
        } else {
            Err(Error::WrongRoute)
        }
    }
    /// A foreign connection with the same numeric bindings is never dispatched,
    /// closed or used as evidence that the original connection has drained.
    pub fn check(&self, q: &QuicRecords) -> Result<(), Error> {
        self.original(q)?;
        if q.is_closed() || !self.attachment.is_complete() {
            return Err(Error::Closed);
        }
        if self.attachment.completed_on(q)? != self.routes
            || !q.has_route(Route::Stream(self.routes.outbound))
            || !q.has_route(Route::Stream(self.routes.inbound))
            || !self
                .routes
                .datagram
                .is_some_and(|r| q.has_route(Route::Datagram(r)))
            || q.receive_ended(self.routes.inbound)?
            || q.receive_ended(self.attachment.control.inbound)?
        {
            return Err(Error::Closed);
        }
        Ok(())
    }
    /// Positive evidence that only this exact optional pair was retired while
    /// its original parent control pair remains open. A generic `Closed` error
    /// also covers parent EOF and cannot establish safe lane-only isolation.
    /// Sample this BEFORE cleanup when classifying callback-time retirement.
    pub fn is_retired(&self, q: &QuicRecords) -> Result<bool, Error> {
        self.original(q)?;
        let retired = self.attachment.retired_on(q)?;
        let control = self.attachment.control;
        if !q.has_route(Route::Stream(control.inbound))
            || !q.has_route(Route::Stream(control.outbound))
            || q.receive_ended(control.inbound)?
        {
            return Err(Error::Closed);
        }
        let outgoing = q
            .native
            .as_ref()
            .ok_or(Error::Closed)?
            .connection()
            .inner()
            .streams()
            .stream(control.outbound.stream)
            .map_err(|_| Error::Native)?;
        if outgoing.send_final_size.is_some()
            || outgoing.send_reset.is_some()
            || outgoing.stop_sending_error_code.is_some()
        {
            return Err(Error::Closed);
        }
        Ok(retired)
    }
    /// Keep the original control record's authority/deadline until the native
    /// empty-send witness releases its queued and retransmittable bytes. Also
    /// usable after retirement; reset tombstones cannot create a fresh send.
    pub fn control_send_drained(&self, q: &QuicRecords) -> Result<bool, Error> {
        self.original(q)?;
        if q.is_closed() {
            return Ok(true);
        }
        q.senders
            .iter()
            .find(|sender| sender.route == self.routes.outbound)
            .map(|sender| sender.records == 0 && sender.bytes == 0 && !q.native_io_unsettled)
            .ok_or(Error::WrongRoute)
    }
    /// No outbound native datagrams or unresolved protected output remain. This
    /// is a conservative connection-wide witness because the native queue can
    /// mix microphone, video and pointer records. It is not a peer playback ACK.
    ///
    /// Retain each packet's immutable egress permit until this is true. If it
    /// expires first, fence the ORIGINAL connection before any further native
    /// I/O: removing a route cannot retract already admitted native datagrams.
    /// Closure and retirement can be queried without restoring the removed lane.
    pub fn datagrams_send_drained(&self, q: &QuicRecords) -> Result<bool, Error> {
        self.original(q)?;
        Ok(q.native.as_ref().is_none_or(|native| {
            !q.native_io_unsettled
                && native
                    .connection()
                    .inner()
                    .pending_outbound_datagram_count()
                    == 0
        }))
    }
    /// Whole-record admission only. Record-specific refusal admits no bytes and
    /// leaves connection authority intact; the original absolute deadline must
    /// survive all backpressure retries and native queue custody.
    pub fn send_control_prepared<E>(
        &self,
        q: &mut QuicRecords,
        cx: &Cx,
        bytes: &[u8],
        until: u64,
        authorize: impl FnMut() -> bool,
        prepared: impl FnMut() -> Result<(), E>,
    ) -> Result<SendAdmission<E>, Error> {
        self.check(q)?;
        let direction = match (self.side, audio::record_kind(bytes)) {
            (Side::Controller, Some(0x0060)) => {
                audio::decode_configuration(bytes, self.binding()).map(|m| m.direction)
            }
            (Side::Host, Some(0x0061)) => {
                audio::decode_configured(bytes, self.binding()).map(|m| m.direction)
            }
            (_, Some(0x0063)) => audio::decode_stop(bytes, self.binding()).map(|m| m.direction),
            _ => return Err(Error::WrongRoute),
        }
        .map_err(|_| Error::Malformed)?;
        if direction != AudioDirection::Uplink {
            return Err(Error::WrongRoute);
        }
        q.send_prepared(
            cx,
            Route::Stream(self.routes.outbound),
            bytes,
            until,
            authorize,
            prepared,
        )
    }
    pub fn send_packet_prepared<E>(
        &self,
        q: &mut QuicRecords,
        cx: &Cx,
        bytes: &[u8],
        until: u64,
        authorize: impl FnMut() -> bool,
        prepared: impl FnMut() -> Result<(), E>,
    ) -> Result<SendAdmission<E>, Error> {
        self.check(q)?;
        let route = self.routes.datagram.ok_or(Error::WrongRoute)?;
        if self.side != Side::Controller || !route.outbound {
            return Err(Error::WrongRoute);
        }
        let packet = audio::decode_packet(bytes, self.binding()).map_err(|_| Error::Malformed)?;
        if packet.direction != AudioDirection::Uplink {
            return Err(Error::WrongRoute);
        }
        q.send_prepared(
            cx,
            Route::Datagram(route),
            bytes,
            until,
            authorize,
            prepared,
        )
    }
    /// At most one handler invocation on this exact lane. Readiness is per route
    /// so native packet backpressure does not block reliable mute/configuration.
    /// Blocked records retain their original transport bytes and receipt/expiry.
    ///
    /// Framing/kind/binding checks belong to QUIC. The coordinator decodes audio
    /// semantics and can consume a malformed owned record before retiring only
    /// this optional lane. A handler error follows QUIC's connection-fencing rule.
    pub fn dispatch(
        &self,
        q: &mut QuicRecords,
        cx: &Cx,
        authorize: impl FnMut() -> bool,
        mut ready: impl FnMut(Route) -> bool,
        mut consume: impl FnMut(Route, &[u8], Option<DatagramReceipt>) -> Result<Disposition, ()>,
    ) -> Result<(), Error> {
        self.check(q)?;
        let available = Cell::new(true);
        q.receive_ready_with_receipts(
            cx,
            authorize,
            |route| available.get() && self.owns_inbound(route) && ready(route),
            |route, bytes, receipt| {
                available.set(false);
                consume(route, bytes, receipt)
            },
        )?;
        self.check(q)
    }
    pub fn request_retirement(&self) {
        self.attachment.request_retirement();
    }
    /// Retain only a one-way retirement request for an I/O authorization guard.
    /// This does not clone the channel or permit dispatch, sending or reattachment.
    pub fn retirement_request(&self) -> RetirementRequest {
        self.attachment.retirement_request()
    }
    pub fn retire(&mut self, q: &mut QuicRecords, cx: &Cx) -> Result<(), Error> {
        self.attachment.retire_audio(q, cx)
    }
    /// Abandonment fences the original connection at its next checked operation.
    pub fn close(&mut self) {
        self.attachment.close();
    }
}
