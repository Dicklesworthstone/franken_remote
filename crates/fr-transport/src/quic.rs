//! FRD0 records over an ACTUAL authenticated Asupersync QUIC connection.
use asupersync::{
    bytes::Bytes,
    cx::Cx,
    net::quic_native::{
        NativeQuicUdpConnection, QuicConnectionState, QuicStream, StreamDirection, StreamId,
        StreamRole,
    },
    time::timeout,
};
use fr_wire::{
    HEADER_BYTES,
    stream::{RecordStream, StreamError},
};
use std::{
    collections::VecDeque,
    fmt,
    sync::{Arc, Weak},
    time::Duration,
};

pub const ALPN: &[u8] = b"fr-remote/0";
const MAX_STREAMS: usize = 8;
const MAX_DATAGRAMS: usize = 4;
const TURN_RECORDS: usize = 16;
/// Upper bound from the pinned native 1200-byte protected packet profile:
/// short-header byte + maximal CID (20) + PN (4) + AEAD (16) + DATAGRAM (9).
/// The actual peer-negotiated DATAGRAM limit can be lower and remains enforced
/// by Asupersync. This is a conservative APPLICATION-record cap, not an MTU.
pub const MAX_DATAGRAM_RECORD: usize = 1150;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    InvalidPolicy,
    NotEstablished,
    Alpn,
    WrongRoute,
    Malformed,
    TooLarge,
    Backpressure,
    Expired,
    Unauthorized,
    Cancelled,
    Closed,
    Stream(StreamError),
    Native,
    Handler,
    Clock,
    Allocation,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Error {}
impl From<StreamError> for Error {
    fn from(e: StreamError) -> Self {
        Self::Stream(e)
    }
}

/// An admitted reliable stream's message family. Input transitions share ONE
/// ordered stream; assigning a stream per key/button/text kind would reorder
/// external effects. This is not a wildcard for unknown message classes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Messages {
    Exact(u16),
    InputActions,
}
impl Messages {
    fn contains(self, kind: u16) -> bool {
        match self {
            Self::Exact(expected) => kind == expected,
            // HeldState (0x0046) is not implemented; InputMode (0x0047)
            // is an ordered action in the existing fr-wire input codec.
            Self::InputActions => matches!(kind, 0x0040 | 0x0041 | 0x0043..=0x0045 | 0x0047),
        }
    }
}
/// Locally selected traffic class, never chosen by a peer's record flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    Critical,
    Bulk,
}
/// Authenticated local route: stream initiator/direction agree with TLS role.
/// A binding may have one stream in EACH direction; input/result share the
/// same application binding without admitting a second parallel action stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamRoute {
    pub stream: StreamId,
    pub binding: u32,
    pub messages: Messages,
    pub priority: Priority,
    pub outbound: bool,
    pub maximum: usize,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DatagramRoute {
    pub binding: u32,
    pub kind: u16,
    pub outbound: bool,
}
#[derive(Debug, Clone, Copy)]
pub struct Policy {
    pub stream_window: u64,
    pub connection_window: u64,
    pub retained_send_bytes: usize,
    pub retained_send_records: usize,
    /// Separate bounded storage that bulk records cannot occupy.
    pub critical_send_bytes: usize,
    pub critical_send_records: usize,
    /// Leave this much actual native connection credit for critical streams.
    pub critical_connection_credit: u64,
    pub datagram_record_bytes: usize,
    pub record_lifetime_micros: u64,
}
impl Default for Policy {
    fn default() -> Self {
        Self {
            stream_window: 65536,
            connection_window: 524_288,
            retained_send_bytes: 131_072,
            retained_send_records: 128,
            critical_send_bytes: 8192,
            critical_send_records: 16,
            critical_connection_credit: 8192,
            datagram_record_bytes: MAX_DATAGRAM_RECORD,
            record_lifetime_micros: 2_000_000,
        }
    }
}
/// Pinned 0.4.10 exposes stream structural equality but not a retained-byte
/// getter. Preserve an EMPTY, never-used outbound stream as an exact absence
/// witness. Refresh only public scalar counters, never clone live private maps.
/// Equality proves that BOTH pending and retained retransmission maps are empty;
/// ACK-only traffic in `bytes_in_flight` is irrelevant. Additional unequal private
/// state conservatively prevents credit reclamation. Requalify when upgrading
/// Asupersync; replace with a public retention query when one is provided.
struct EmptySendState(QuicStream);
impl EmptySendState {
    fn matches(&mut self, live: &QuicStream) -> bool {
        self.0.send_offset = live.send_offset;
        self.0.recv_offset = live.recv_offset;
        self.0.read_offset = live.read_offset;
        self.0.send_credit = live.send_credit.clone();
        self.0.recv_credit = live.recv_credit.clone();
        // A terminated stream is not reusable even if its buffers are empty.
        if live.send_final_size.is_some()
            || live.send_reset.is_some()
            || live.stop_sending_error_code.is_some()
        {
            return false;
        }
        self.0 == *live
    }
}
struct Sender {
    route: StreamRoute,
    empty: EmptySendState,
    bytes: usize,
    records: usize,
    until: Option<u64>,
}
struct PendingWrite {
    route: StreamRoute,
    bytes: Bytes,
    offset: usize,
    send_by: u64,
}
struct Inbound {
    route: StreamRoute,
    framing: RecordStream,
    remainder: Bytes,
    fin: bool,
}
/// A synchronous handler either consumes the borrowed record or applies
/// backpressure. It must validate its message codec and current session state.
/// It must not block on native capture, input, decoding, or presentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    Consumed,
    Blocked,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    Stream(StreamRoute),
    Datagram(DatagramRoute),
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub framed_capacity: usize,
    pub remainder_bytes: usize,
    pub retained_send_upper_bound: usize,
    pub retained_send_records: usize,
    pub critical_send_bytes: usize,
    pub critical_send_records: usize,
}
/// Opaque identity for one connection owner, never a peer-supplied session ID.
#[derive(Clone)]
pub struct ConnectionBinding(Weak<()>);

/// One exclusive connection owner. In-flight I/O moves the native connection
/// out of this object: dropping that future drops the socket and makes reuse
/// terminal, instead of retrying a partially transmitted record.
///
/// Reliable send accounting is deliberately conservative: credit is released
/// only when native reliable buffers are proven empty, NOT merely when packet
/// assembly drains or an ACK count increases. Batches, not individual frames,
/// on each stream may wait for ACKs; there is no new on-wire per-frame acknowledgement.
/// Asupersync still owns congestion/loss recovery and its qualification gates.
pub struct QuicRecords {
    identity: Arc<()>,
    native: Option<NativeQuicUdpConnection>,
    streams: Vec<StreamRoute>,
    datagrams: Vec<DatagramRoute>,
    inbound: Vec<Inbound>,
    pending_datagram: Option<(DatagramRoute, Bytes, u64)>,
    policy: Policy,
    last_now: Option<u64>,
    read_bytes: u64,
    advertised_limit: u64,
    pending_writes: VecDeque<PendingWrite>,
    senders: Vec<Sender>,
    cursor: usize,
}
impl fmt::Debug for QuicRecords {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QuicRecords")
            .field("closed", &self.is_closed())
            .field("usage", &self.usage())
            .finish_non_exhaustive()
    }
}
impl QuicRecords {
    pub fn new(
        mut native: NativeQuicUdpConnection,
        cx: &Cx,
        streams: &[StreamRoute],
        datagrams: &[DatagramRoute],
        policy: Policy,
    ) -> Result<Self, Error> {
        cx.checkpoint().map_err(|_| Error::Cancelled)?;
        validate_policy(streams, datagrams, policy, native.connection().role())?;
        if native.negotiated_alpn() != ALPN {
            return Err(Error::Alpn);
        }
        if native.connection().state() != QuicConnectionState::Established
            || !native.connection().can_send_app_data()
        {
            return Err(Error::NotEstablished);
        }
        let table = native.connection().inner().streams();
        if table.connection_recv_limit() > policy.connection_window
            || table.len() > streams.len()
            || native
                .connection()
                .inner()
                .pending_outbound_datagram_count()
                != 0
            || native.connection().inner().has_pending_stream_frames()
        {
            return Err(Error::InvalidPolicy);
        }
        let mut inbound = Vec::new();
        let mut senders = Vec::new();
        for route in streams {
            if route.outbound {
                let s = native
                    .connection()
                    .inner()
                    .streams()
                    .stream(route.stream)
                    .map_err(|_| Error::WrongRoute)?;
                if s.send_offset != 0
                    || s.send_final_size.is_some()
                    || s.send_reset.is_some()
                    || s.stop_sending_error_code.is_some()
                {
                    return Err(Error::InvalidPolicy);
                }
                senders.push(Sender {
                    route: *route,
                    empty: EmptySendState(s.clone()),
                    bytes: 0,
                    records: 0,
                    until: None,
                });
            } else {
                // Installing the accepted binding also bounds the native stream
                // before the first application read. Previously received data is
                // rejected: an adapter cannot retroactively undo early admission.
                if let Ok(s) = native.connection().inner().streams().stream(route.stream)
                    && (s.recv_offset != 0 || s.recv_credit.limit() > policy.stream_window)
                {
                    return Err(Error::InvalidPolicy);
                }
                let limit = native
                    .connection_mut()
                    .configure_stream_receive_window(cx, route.stream, policy.stream_window)
                    .map_err(|_| Error::Native)?;
                if limit > policy.stream_window {
                    return Err(Error::InvalidPolicy);
                }
                inbound.push(Inbound {
                    route: *route,
                    framing: RecordStream::new(
                        route.maximum,
                        route.binding,
                        policy.record_lifetime_micros,
                    )?,
                    remainder: Bytes::new(),
                    fin: false,
                });
            }
        }
        Ok(Self {
            identity: Arc::new(()),
            native: Some(native),
            streams: streams.to_vec(),
            datagrams: datagrams.to_vec(),
            inbound,
            pending_datagram: None,
            policy,
            last_now: None,
            read_bytes: 0,
            advertised_limit: policy.connection_window,
            pending_writes: VecDeque::new(),
            senders,
            cursor: 0,
        })
    }
    pub fn binding(&self) -> ConnectionBinding {
        ConnectionBinding(Arc::downgrade(&self.identity))
    }
    /// Object identity only; this does not make a closed connection live.
    pub fn is_bound_to(&self, binding: &ConnectionBinding) -> bool {
        Weak::ptr_eq(&Arc::downgrade(&self.identity), &binding.0)
    }
    pub fn has_route(&self, route: Route) -> bool {
        match route {
            Route::Stream(route) => self.streams.contains(&route),
            Route::Datagram(route) => self.datagrams.contains(&route),
        }
    }
    /// A clean FIN on an admitted input stream is still a session lifecycle
    /// event, not permission to retain held input indefinitely.
    pub fn receive_finished(&self, route: StreamRoute) -> Result<bool, Error> {
        self.inbound
            .iter()
            .find(|s| s.route == route)
            .map(|s| s.fin)
            .ok_or(Error::WrongRoute)
    }
    pub fn is_closed(&self) -> bool {
        self.native.is_none()
    }
    pub fn close(&mut self) {
        self.native = None;
        self.pending_datagram = None;
        self.pending_writes.clear();
        for s in &mut self.inbound {
            s.framing.close();
            s.remainder = Bytes::new();
        }
        for s in &mut self.senders {
            s.bytes = 0;
            s.records = 0;
            s.until = None;
        }
    }
    pub fn usage(&self) -> Usage {
        Usage {
            framed_capacity: self
                .inbound
                .iter()
                .map(|s| s.framing.allocated_bytes())
                .sum(),
            remainder_bytes: self
                .inbound
                .iter()
                .map(|s| s.remainder.len())
                .sum::<usize>()
                + self
                    .pending_datagram
                    .as_ref()
                    .map_or(0, |(_, b, _)| b.len()),
            retained_send_upper_bound: self.senders.iter().map(|s| s.bytes).sum(),
            retained_send_records: self.senders.iter().map(|s| s.records).sum(),
            critical_send_bytes: self.send_usage(Priority::Critical).0,
            critical_send_records: self.send_usage(Priority::Critical).1,
        }
    }
    fn send_usage(&self, priority: Priority) -> (usize, usize) {
        self.senders
            .iter()
            .filter(|s| s.route.priority == priority)
            .fold((0, 0), |(bytes, records), s| {
                (bytes + s.bytes, records + s.records)
            })
    }
    fn check(&mut self, cx: &Cx, authorize: &mut impl FnMut() -> bool) -> Result<u64, Error> {
        if self.is_closed() {
            return Err(Error::Closed);
        }
        if cx.checkpoint().is_err() {
            self.close();
            return Err(Error::Cancelled);
        }
        if !authorize() {
            self.close();
            return Err(Error::Unauthorized);
        }
        let result = (|| {
            let now = now(cx)?;
            if self.last_now.is_some_and(|old| now < old) {
                return Err(Error::Clock);
            }
            self.last_now = Some(now);
            if self
                .senders
                .iter()
                .any(|s| s.until.is_some_and(|until| now >= until))
            {
                return Err(Error::Expired);
            }
            for s in &mut self.inbound {
                if !s.fin {
                    s.framing.tick(now)?;
                }
            }
            Ok(now)
        })();
        if result.is_err() {
            self.close();
        }
        result
    }
    /// All validation precedes the copy; the authority callback and clock are
    /// checked again before bounded admission. Reliable records are sliced into
    /// native stream writes by `drive`, with authority/deadline checks there too.
    /// Backpressure admits no bytes: retain this SAME prepared record for retry.
    pub fn send(
        &mut self,
        cx: &Cx,
        route: Route,
        bytes: &[u8],
        send_by_micros: u64,
        mut authorize: impl FnMut() -> bool,
    ) -> Result<(), Error> {
        let current = self.check(cx, &mut authorize)?;
        if current >= send_by_micros {
            return Err(Error::Expired);
        }
        let native = self.native.as_ref().ok_or(Error::Closed)?;
        match route {
            Route::Stream(r) => {
                if !r.outbound || !self.streams.contains(&r) {
                    return Err(Error::WrongRoute);
                }
                validate_record(bytes, r.maximum, r.binding, r.messages)?;
                let (used_bytes, used_records) = self.send_usage(r.priority);
                let (byte_limit, record_limit) = match r.priority {
                    Priority::Critical => (
                        self.policy.critical_send_bytes,
                        self.policy.critical_send_records,
                    ),
                    Priority::Bulk => (
                        self.policy.retained_send_bytes,
                        self.policy.retained_send_records,
                    ),
                };
                if used_records >= record_limit || bytes.len() > byte_limit - used_bytes {
                    return Err(Error::Backpressure);
                }
            }
            Route::Datagram(r) => {
                if !r.outbound || !self.datagrams.contains(&r) {
                    return Err(Error::WrongRoute);
                }
                validate_record(
                    bytes,
                    self.policy.datagram_record_bytes,
                    r.binding,
                    Messages::Exact(r.kind),
                )?;
                let queued = native
                    .connection()
                    .inner()
                    .pending_outbound_datagram_count();
                let path = native.connection().path_stats();
                let available = path
                    .congestion_window_bytes
                    .saturating_sub(path.bytes_in_flight);
                if queued >= MAX_DATAGRAMS || available < (queued as u64 + 2) * 1200 {
                    return Err(Error::Backpressure);
                }
            }
        }
        let mut storage = Vec::new();
        storage
            .try_reserve_exact(bytes.len())
            .map_err(|_| Error::Allocation)?;
        if storage.capacity() > bytes.len() {
            return Err(Error::Allocation);
        }
        storage.extend_from_slice(bytes);
        if self.check(cx, &mut authorize)? >= send_by_micros {
            return Err(Error::Expired);
        }
        match route {
            Route::Stream(route) => {
                self.pending_writes
                    .try_reserve(1)
                    .map_err(|_| Error::Allocation)?;
                self.pending_writes.push_back(PendingWrite {
                    route,
                    bytes: Bytes::from(storage),
                    offset: 0,
                    send_by: send_by_micros,
                });
                let sender = self
                    .senders
                    .iter_mut()
                    .find(|s| s.route == route)
                    .expect("validated outgoing route");
                sender.bytes += bytes.len();
                sender.records += 1;
                sender.until = Some(
                    sender
                        .until
                        .map_or(send_by_micros, |old| old.min(send_by_micros)),
                );
            }
            Route::Datagram(_) => {
                if self
                    .native
                    .as_mut()
                    .ok_or(Error::Closed)?
                    .connection_mut()
                    .send_datagram(cx, Bytes::from(storage))
                    .is_err()
                {
                    self.close();
                    return Err(Error::Native);
                }
            }
        }
        Ok(())
    }
    /// Flush bounded native packets, then wait at most `wait` for incoming I/O.
    /// The Asupersync reactor/loss timers do the waiting. Dropping this operation
    /// is terminal. The external session watchdog must independently cancel Cx
    /// on authority expiry/revoke, including while this future is suspended.
    pub async fn drive(
        &mut self,
        cx: &Cx,
        wait: Duration,
        mut authorize: impl FnMut() -> bool,
    ) -> Result<(), Error> {
        self.check(cx, &mut authorize)?;
        if wait > Duration::from_millis(100) {
            return Err(Error::InvalidPolicy);
        }
        let current = self.check(cx, &mut authorize)?;
        if let Err(error) = self.queue_stream_prefix(cx, current) {
            self.close();
            return Err(error);
        }
        let mut native = self.native.take().ok_or(Error::Closed)?;
        let overall = Duration::from_millis(250);
        let result = timeout(cx.now(), overall, async {
            native.flush(cx).await.map_err(|_| Error::Native)?;
            if cx.checkpoint().is_err() {
                return Err(Error::Cancelled);
            }
            if !authorize() {
                return Err(Error::Unauthorized);
            }
            native
                .drive_io_once(cx, wait)
                .await
                .map_err(|_| Error::Native)?;
            Ok(())
        })
        .await
        .map_err(|_| Error::Expired)
        .and_then(core::convert::identity);
        if let Err(e) = result {
            self.close();
            return Err(e);
        }
        self.native = Some(native);
        self.check(cx, &mut authorize)?;
        let native = self.native.as_mut().ok_or(Error::Closed)?;
        if native.connection().state() != QuicConnectionState::Established
            || native.connection().inner().streams().len() > self.streams.len()
        {
            self.close();
            return Err(Error::Closed);
        }
        // Pending bytes alone exclude retransmission copies. Prove actual
        // stream-buffer absence; total flight includes unrelated ACK/control.
        for sender in &mut self.senders {
            if !self.pending_writes.iter().any(|p| p.route == sender.route)
                && native
                    .connection()
                    .inner()
                    .streams()
                    .stream(sender.route.stream)
                    .is_ok_and(|live| sender.empty.matches(live))
            {
                // A stalled bulk stream must not hold already acknowledged
                // control receipts against the critical storage reservation.
                sender.bytes = 0;
                sender.records = 0;
                sender.until = None;
            }
        }
        Ok(())
    }
    /// Stage one small prefix, leaving congestion/loss control with Asupersync.
    /// Large application records are NOT single QUIC frames. No prefix is staged
    /// when native queued work plus flight could fill the protected-packet window.
    fn queue_stream_prefix(&mut self, cx: &Cx, now: u64) -> Result<(), Error> {
        if self.pending_writes.iter().any(|p| now >= p.send_by) {
            return Err(Error::Expired);
        }
        let native = self.native.as_mut().ok_or(Error::Closed)?;
        let inner = native.connection().inner();
        let path = native.connection().path_stats();
        let needed = (inner.pending_outbound_datagram_count() as u64 + 2) * 1200;
        if inner.has_pending_stream_frames()
            || path
                .congestion_window_bytes
                .saturating_sub(path.bytes_in_flight)
                < needed
        {
            return Ok(());
        }
        let reserve = if self
            .senders
            .iter()
            .any(|s| s.route.priority == Priority::Critical)
        {
            self.policy.critical_connection_credit
        } else {
            0
        };
        let connection_credit = inner.streams().connection_send_remaining();
        let mut selected = None;
        for priority in [Priority::Critical, Priority::Bulk] {
            for (index, pending) in self.pending_writes.iter().enumerate() {
                // Preserve bytes and whole-record order WITHIN each stream,
                // while a flow-blocked stream cannot block another stream.
                if pending.route.priority != priority
                    || self
                        .pending_writes
                        .iter()
                        .take(index)
                        .any(|p| p.route.stream == pending.route.stream)
                {
                    continue;
                }
                let credit = inner
                    .stream_send_credit_remaining(pending.route.stream)
                    .min(if priority == Priority::Bulk {
                        connection_credit.saturating_sub(reserve)
                    } else {
                        connection_credit
                    });
                let length = (pending.bytes.len() - pending.offset)
                    .min(900)
                    .min(usize::try_from(credit).unwrap_or(usize::MAX));
                if length != 0 {
                    selected = Some((index, length));
                    break;
                }
            }
            if selected.is_some() {
                break;
            }
        }
        let Some((index, length)) = selected else {
            return Ok(());
        };
        let pending = &mut self.pending_writes[index];
        native
            .connection_mut()
            .write_stream(
                cx,
                pending.route.stream,
                pending.bytes.slice(pending.offset..pending.offset + length),
                false,
            )
            .map_err(|_| Error::Native)?;
        pending.offset += length;
        if pending.offset == pending.bytes.len() {
            self.pending_writes.remove(index);
        }
        Ok(())
    }
    /// Drain at most 16 borrowed records per turn, alternating streams with
    /// datagrams. A blocked handler keeps its complete record (one per route).
    /// Native receive windows plus these explicitly reported buffers must be
    /// included in the parent process's admission budget.
    pub fn receive(
        &mut self,
        cx: &Cx,
        mut authorize: impl FnMut() -> bool,
        mut handler: impl FnMut(Route, &[u8]) -> Result<Disposition, ()>,
    ) -> Result<usize, Error> {
        self.receive_ready(cx, &mut authorize, |_| true, &mut handler)
    }
    /// Read only lanes whose bounded consumers can accept a record. A blocked
    /// lane keeps its native flow credit rather than copying bytes merely to
    /// advertise a larger receive window. Poll other lanes and expiry normally.
    pub fn receive_ready(
        &mut self,
        cx: &Cx,
        mut authorize: impl FnMut() -> bool,
        mut ready: impl FnMut(Route) -> bool,
        mut handler: impl FnMut(Route, &[u8]) -> Result<Disposition, ()>,
    ) -> Result<usize, Error> {
        let result = self.receive_inner(cx, &mut authorize, &mut ready, &mut handler);
        if result.is_err() {
            self.close();
        }
        result
    }
    fn receive_datagram(
        &mut self,
        current: u64,
        handler: &mut impl FnMut(Route, &[u8]) -> Result<Disposition, ()>,
    ) -> Result<usize, Error> {
        if self.pending_datagram.is_none() {
            let Some(bytes) = self
                .native
                .as_mut()
                .ok_or(Error::Closed)?
                .connection_mut()
                .recv_datagram()
            else {
                return Ok(0);
            };
            if bytes.len() < HEADER_BYTES || bytes.len() > self.policy.datagram_record_bytes {
                return Err(Error::TooLarge);
            }
            let binding =
                u32::from_be_bytes(bytes[16..20].try_into().map_err(|_| Error::Malformed)?);
            let kind = u16::from_be_bytes([bytes[6], bytes[7]]);
            let Some(route) = self
                .datagrams
                .iter()
                .find(|r| !r.outbound && r.binding == binding)
                .copied()
            else {
                // Late datagrams for retired bindings are disposable. No
                // response allocation and no reinterpretation as current.
                return Ok(0);
            };
            if kind != route.kind {
                return Err(Error::WrongRoute);
            }
            validate_record(
                &bytes,
                self.policy.datagram_record_bytes,
                binding,
                Messages::Exact(kind),
            )?;
            let until = current
                .checked_add(self.policy.record_lifetime_micros)
                .ok_or(Error::Clock)?;
            self.pending_datagram = Some((route, bytes, until));
        }
        let (route, bytes, until) = self.pending_datagram.as_ref().expect("installed");
        if current >= *until {
            self.pending_datagram = None;
            return Ok(0);
        }
        if handler(Route::Datagram(*route), bytes).map_err(|()| Error::Handler)?
            == Disposition::Consumed
        {
            self.pending_datagram = None;
            return Ok(1);
        }
        Ok(0)
    }
    fn receive_inner(
        &mut self,
        cx: &Cx,
        authorize: &mut impl FnMut() -> bool,
        ready: &mut impl FnMut(Route) -> bool,
        handler: &mut impl FnMut(Route, &[u8]) -> Result<Disposition, ()>,
    ) -> Result<usize, Error> {
        let mut count = 0;
        let lanes = self.inbound.len() + 1;
        for _ in 0..TURN_RECORDS {
            let current = self.check(cx, authorize)?;
            let which = self.cursor % lanes;
            self.cursor = (self.cursor + 1) % lanes;
            if which == self.inbound.len() {
                count += self.receive_datagram(current, &mut |route, bytes| {
                    if ready(route) {
                        handler(route, bytes)
                    } else {
                        Ok(Disposition::Blocked)
                    }
                })?;
            } else {
                let s = &mut self.inbound[which];
                if s.fin || !ready(Route::Stream(s.route)) {
                    continue;
                }
                if s.framing.frame(current)?.is_none() {
                    if s.remainder.is_empty() {
                        s.remainder = self
                            .native
                            .as_mut()
                            .ok_or(Error::Closed)?
                            .connection_mut()
                            .read_stream(cx, s.route.stream, s.route.maximum)
                            .map_err(|_| Error::Native)?;
                        self.read_bytes = self
                            .read_bytes
                            .checked_add(s.remainder.len() as u64)
                            .ok_or(Error::Clock)?;
                    }
                    let n = s.framing.push(&s.remainder, current)?;
                    s.remainder = s.remainder.slice(n..);
                }
                if let Some(bytes) = s.framing.frame(current)? {
                    validate_record(bytes, s.route.maximum, s.route.binding, s.route.messages)?;
                    if handler(Route::Stream(s.route), bytes).map_err(|()| Error::Handler)?
                        == Disposition::Consumed
                    {
                        s.framing.consume(current)?;
                        count += 1;
                    }
                }
                if s.remainder.is_empty()
                    && s.framing.frame(current)?.is_none()
                    && self
                        .native
                        .as_ref()
                        .ok_or(Error::Closed)?
                        .connection()
                        .is_stream_eof(s.route.stream)
                        .map_err(|_| Error::Native)?
                {
                    s.framing.finish(current)?;
                    s.fin = true;
                }
            }
        }
        // QUIC connection flow control counts all reliable application bytes.
        // Advance from actual drained bytes, not arrival, ACK, or decode reports.
        let limit = self
            .read_bytes
            .checked_add(self.policy.connection_window)
            .ok_or(Error::Clock)?;
        if limit > self.advertised_limit {
            self.native
                .as_mut()
                .ok_or(Error::Closed)?
                .connection_mut()
                .advertise_connection_receive_limit(cx, limit)
                .map_err(|_| Error::Native)?;
            self.advertised_limit = limit;
        }
        Ok(count)
    }
}
fn now(cx: &Cx) -> Result<u64, Error> {
    Ok(cx.timer_driver().ok_or(Error::Clock)?.now().as_nanos() / 1000)
}
fn validate_policy(
    streams: &[StreamRoute],
    datagrams: &[DatagramRoute],
    p: Policy,
    role: StreamRole,
) -> Result<(), Error> {
    if streams.len() > MAX_STREAMS
        || datagrams.len() > MAX_DATAGRAMS
        || !(1..=65536).contains(&p.stream_window)
        || !(p.stream_window..=524_288).contains(&p.connection_window)
        || !(1..=1_048_576).contains(&p.retained_send_bytes)
        || !(1..=1024).contains(&p.retained_send_records)
        || !(HEADER_BYTES..=65536).contains(&p.critical_send_bytes)
        || !(1..=128).contains(&p.critical_send_records)
        || !(1..=p.connection_window).contains(&p.critical_connection_credit)
        || !(HEADER_BYTES..=MAX_DATAGRAM_RECORD).contains(&p.datagram_record_bytes)
        || !(1..=5_000_000).contains(&p.record_lifetime_micros)
    {
        return Err(Error::InvalidPolicy);
    }
    for (i, r) in streams.iter().enumerate() {
        if r.binding == 0
            || matches!(r.messages, Messages::Exact(0))
            || (r.messages == Messages::InputActions
                && (!r.stream.is_local_for(StreamRole::Client) || r.priority != Priority::Critical))
            || !(HEADER_BYTES..=65536).contains(&r.maximum)
            || r.maximum as u64 > p.stream_window
            || r.maximum
                > match r.priority {
                    Priority::Critical => p.critical_send_bytes,
                    Priority::Bulk => p.retained_send_bytes,
                }
            || r.stream.direction() != StreamDirection::Unidirectional
            || r.stream.is_local_for(role) != r.outbound
            || streams[..i].iter().any(|old| {
                old.stream == r.stream || (old.binding == r.binding && old.outbound == r.outbound)
            })
        {
            return Err(Error::InvalidPolicy);
        }
    }
    for (i, r) in datagrams.iter().enumerate() {
        if r.binding == 0
            || r.kind == 0
            || datagrams[..i]
                .iter()
                .any(|old| old.binding == r.binding && old.outbound == r.outbound)
            || streams.iter().any(|old| {
                old.binding == r.binding
                    && old.outbound == r.outbound
                    && !(old.messages == Messages::InputActions && r.kind == 0x0042)
            })
            || (r.kind == 0x0042 && r.outbound != (role == StreamRole::Client))
        {
            return Err(Error::InvalidPolicy);
        }
    }
    Ok(())
}
fn validate_record(
    bytes: &[u8],
    maximum: usize,
    binding: u32,
    messages: Messages,
) -> Result<(), Error> {
    if bytes.len() < HEADER_BYTES {
        return Err(Error::Malformed);
    }
    if bytes.len() > maximum {
        return Err(Error::TooLarge);
    }
    if bytes[..6] != *b"FRD0\0\0"
        || bytes[8..12] != [0; 4]
        || !messages.contains(u16::from_be_bytes([bytes[6], bytes[7]]))
        || u32::from_be_bytes(bytes[16..20].try_into().map_err(|_| Error::Malformed)?) != binding
    {
        return Err(Error::WrongRoute);
    }
    let total = u32::from_be_bytes(bytes[12..16].try_into().map_err(|_| Error::Malformed)?);
    let extensions = u32::from_be_bytes(bytes[20..24].try_into().map_err(|_| Error::Malformed)?);
    if extensions > total
        || usize::try_from(total)
            .ok()
            .and_then(|n| n.checked_add(HEADER_BYTES))
            != Some(bytes.len())
    {
        return Err(Error::Malformed);
    }
    Ok(())
}
