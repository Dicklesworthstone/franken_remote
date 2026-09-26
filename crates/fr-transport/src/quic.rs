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

mod lifetime;
pub use lifetime::terminal::{
    CloseOutcome, ClosedRegistration, ClosedReport, RevocationRegistration, RevocationReport,
};

pub mod budget;
pub mod clipboard;
pub mod files;
pub use budget::{BudgetError, IpVersion, PacketBudget};

mod attachment;
pub use attachment::{AttachedChannel, ChannelRequest, ChannelScope, MediaChannel};

mod startup;
pub use startup::ControlRoutes;

pub const ALPN: &[u8] = b"fr-remote/0";
const MAX_STREAMS: usize = 16;
/// Datagram ROUTES per connection (not queued records).
const MAX_DATAGRAMS: usize = 4;
const TURN_RECORDS: usize = 16;
/// Outgoing datagram records that may wait in the native queue for the next
/// `drive` flush: one turn's records, as for incoming records. Senders refill
/// this queue only between drives, and each drive then waits for ingress, so a
/// smaller bound spreads one multi-fragment picture over one receive wait per
/// few fragments (4 let 23 fragments take ~40 ms and miss the receiver's 50 ms
/// display budget, which starts at the first fragment). Congestion credit still
/// gates every admission.
const MAX_QUEUED_DATAGRAMS: usize = TURN_RECORDS;
/// Keep a turn bounded while allowing queued bulk records to use available
/// congestion credit without one idle receive timeout per 900-byte prefix.
const TURN_SEND_PREFIXES: usize = 8;
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
    /// Attachment completed, but this direction has no application payloads.
    /// The stream remains owned and monitored for FIN/RESET, never recycled.
    NoApplication,
    Exact(u16),
    InputActions,
    /// Host tickets and terminal input results on the same reliable feedback lane.
    InputFeedback,
    /// Only bounded Begin/Chunk/Commit/Cancel records, on a dedicated pair.
    Clipboard,
    /// Separately attached file records, never an input/control parser exception.
    Files,
    /// Decoder configuration and first-frame acknowledgements, one ordered lane.
    DecoderReplies,
    /// The host's reliable media-configuration lane: `DecoderConfiguration`
    /// and, only after `remote-cursor` selection, `CursorShape`. The session
    /// still refuses a shape the peer did not negotiate.
    MediaConfiguration,
    /// The host's reliable `audio-down` lane: `AudioConfiguration` and
    /// `AudioStop` only. `AudioPacket` never enters a reliable stream.
    AudioControl,
    /// The viewer's reliable `audio-down` reply lane: `AudioConfigured` and a
    /// receiver-side `AudioStop`. Neither can enable the other direction.
    AudioReplies,
    /// Initial native control only, before the host installs a binding.
    Negotiation,
    /// Bound connection control. The session codec still checks kind/state.
    SessionControl,
}
impl Messages {
    fn contains(self, kind: u16) -> bool {
        match self {
            Self::NoApplication => false,
            Self::Exact(expected) => kind == expected,
            Self::InputFeedback => matches!(kind, 0x0017 | 0x0048),
            Self::Clipboard => matches!(kind, 0x0050..=0x0053),
            Self::Files => matches!(kind, 0x0070..=0x0074),
            Self::DecoderReplies => matches!(kind, 0x0031 | 0x0033),
            Self::MediaConfiguration => matches!(kind, 0x0030 | 0x0038),
            Self::AudioControl => matches!(kind, 0x0060 | 0x0063),
            Self::AudioReplies => matches!(kind, 0x0061 | 0x0063),
            Self::Negotiation => matches!(kind, 0x0001..=0x0004 | 0x0010 | 0x0011),
            Self::SessionControl => {
                matches!(
                    kind,
                    0x0004 | 0x0012
                        ..=0x001e
                            | 0x0020
                            | 0x0022
                            | 0x0036
                            | 0x0054
                            | 0x0080
                            | 0x0082
                            | 0x0084
                            | 0x0085
                )
            }
            // Release-only HeldState and InputMode share the ordered action
            // stream. Neither pointer datagrams nor results enter this lane.
            Self::InputActions => matches!(kind, 0x0040 | 0x0041 | 0x0043..=0x0047),
        }
    }
}
/// Kinds admitted on one installed datagram route. The video route carries
/// both Video-channel kinds: access-unit fragments and replaceable cursor
/// positions. The session still refuses a position its peer did not negotiate.
fn datagram_admits(route_kind: u16, kind: u16) -> bool {
    kind == route_kind || (route_kind == 0x0034 && kind == 0x0039)
}
/// Full validation of one datagram record against its installed route.
fn validate_datagram(bytes: &[u8], maximum: usize, route: DatagramRoute) -> Result<(), Error> {
    let kind = bytes
        .get(6..8)
        .map(|k| u16::from_be_bytes([k[0], k[1]]))
        .ok_or(Error::Malformed)?;
    if !datagram_admits(route.kind, kind) {
        return Err(Error::WrongRoute);
    }
    validate_record(bytes, maximum, route.binding, Messages::Exact(kind))
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
impl Policy {
    /// Validate resource bounds before allocating a socket or TLS handshake.
    pub fn validate(self) -> Result<(), Error> {
        validate_policy(&[], &[], self, StreamRole::Client)
    }
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
// A closed set of records staged to native STREAM buffers. New admissions stay
// in pending_writes until this epoch is proven absent from BOTH native pending
// and retransmission storage. Continuous traffic cannot pin an acknowledged old
// deadline forever; no acknowledgement is guessed from queue size or flight.
struct SendEpoch {
    bytes: usize,
    records: usize,
}
struct Sender {
    route: StreamRoute,
    empty: EmptySendState,
    bytes: usize,
    records: usize,
    until: Option<u64>,
    epoch: Option<SendEpoch>,
}
struct PendingWrite {
    route: StreamRoute,
    bytes: Bytes,
    offset: usize,
    send_by: u64,
    in_epoch: bool,
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
    // Drop order: the exact input fence precedes native socket destruction.
    deferred_revocation: Option<Box<lifetime::terminal::Armed>>,
    lifetime_check: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
    terminal_lifetime_check: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
    clock_attached: bool,
    display_selection_claimed: bool,
    attachments: Vec<attachment::Reservation>,
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
                    epoch: None,
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
                    framing: if route.messages == Messages::Negotiation {
                        RecordStream::negotiation(route.maximum, policy.record_lifetime_micros)?
                    } else {
                        RecordStream::new(
                            route.maximum,
                            route.binding,
                            policy.record_lifetime_micros,
                        )?
                    },
                    remainder: Bytes::new(),
                    fin: false,
                });
            }
        }
        Ok(Self {
            identity: Arc::new(()),
            lifetime_check: None,
            terminal_lifetime_check: None,
            deferred_revocation: None,
            clock_attached: false,
            display_selection_claimed: false,
            attachments: Vec::new(),
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
            Route::Stream(route) => {
                self.streams.contains(&route)
                    && !self
                        .attachments
                        .iter()
                        .any(|r| r.binding == route.binding && r.retired())
            }
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
    /// Authenticated peer FIN/RESET, regardless of unread data or consumer
    /// backpressure. Input owners must fence on this before dispatching more
    /// effects. This is NOT proof that framing completed or native work drained;
    /// `receive_finished` retains its separate consumed-through-FIN meaning.
    pub fn receive_ended(&self, route: StreamRoute) -> Result<bool, Error> {
        if !self.inbound.iter().any(|s| s.route == route) {
            return Err(Error::WrongRoute);
        }
        let stream = self
            .native
            .as_ref()
            .ok_or(Error::Closed)?
            .connection()
            .inner()
            .streams()
            .stream(route.stream)
            .map_err(|_| Error::Native)?;
        Ok(stream.final_size.is_some()
            || stream.recv_reset.is_some()
            || self
                .attachments
                .iter()
                .any(|r| r.inbound == route.stream && r.retired()))
    }
    pub fn is_closed(&self) -> bool {
        self.native.is_none()
    }
    pub fn close(&mut self) {
        self.capture_revocation();
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
            s.epoch = None;
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
    fn datagram_maximum(&self, route: DatagramRoute) -> usize {
        self.attachments
            .iter()
            .find(|a| a.binding == route.binding)
            .and_then(|a| a.datagram_maximum)
            .unwrap_or(self.policy.datagram_record_bytes)
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
        if self.lifetime_check.as_ref().is_some_and(|check| !check()) || !authorize() {
            self.close();
            return Err(Error::Unauthorized);
        }
        let result = (|| {
            let now = now(cx)?;
            if self.last_now.is_some_and(|old| now < old) {
                return Err(Error::Clock);
            }
            self.last_now = Some(now);
            self.service_optional_retirements(cx)?;
            if self.attachments.iter().any(|r| r.expired(now)) {
                return Err(Error::Expired);
            }
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
    /// Service cancellation, admission and retained-record deadlines without
    /// reading another stream or waiting for network traffic.
    pub fn tick(&mut self, cx: &Cx, mut authorize: impl FnMut() -> bool) -> Result<(), Error> {
        self.check(cx, &mut authorize).map(|_| ())
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
                if !r.outbound || !self.has_route(Route::Stream(r)) {
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
                validate_datagram(bytes, self.datagram_maximum(r), r)?;
                let queued = native
                    .connection()
                    .inner()
                    .pending_outbound_datagram_count();
                let path = native.connection().path_stats();
                let available = path
                    .congestion_window_bytes
                    .saturating_sub(path.bytes_in_flight);
                if queued >= MAX_QUEUED_DATAGRAMS || available < (queued as u64 + 2) * 1200 {
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
                    in_epoch: false,
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
    /// Flush at most eight small bulk prefixes or one critical prefix, then wait
    /// at most `wait` for incoming I/O. Each prefix rechecks authority, deadlines, native congestion
    /// and flow credit, and critical-stream priority. The whole turn retains one
    /// 250 ms bound; backpressure never becomes an unbounded write loop.
    /// Dropping an active native I/O operation is terminal. The session watchdog
    /// must independently cancel Cx on expiry/revoke, including during silence.
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
        let result = timeout(
            cx.now(),
            Duration::from_millis(250),
            self.drive_turn(cx, wait, &mut authorize),
        )
        .await
        .map_err(|_| Error::Expired)
        .and_then(core::convert::identity);
        if result.is_err() {
            self.close();
        }
        result
    }
    async fn drive_turn(
        &mut self,
        cx: &Cx,
        wait: Duration,
        authorize: &mut impl FnMut() -> bool,
    ) -> Result<(), Error> {
        let gate = self.lifetime_check.clone();
        for _ in 0..TURN_SEND_PREFIXES {
            let current = self.check(cx, authorize)?;
            let queued = self.queue_stream_prefix(cx, current)?;
            let until = self.senders.iter().filter_map(|s| s.until).min();
            // Native ownership stays outside self across every await: dropping
            // or unwinding an in-flight operation cannot resume its queued data.
            let mut native = self.native.take().ok_or(Error::Closed)?;
            let result = lifetime::poll_io(
                cx,
                gate.as_deref(),
                authorize,
                current,
                until,
                native.flush(cx),
            )
            .await;
            self.finish_native_io(native, result)?;
            if queued != Some(Priority::Bulk) {
                // A critical prefix gets the first native slot, then yields to
                // receive/service control before spending a bulk turn budget.
                break;
            }
        }
        let current = self.check(cx, authorize)?;
        let until = self.senders.iter().filter_map(|s| s.until).min();
        let mut native = self.native.take().ok_or(Error::Closed)?;
        let result = lifetime::poll_io(
            cx,
            gate.as_deref(),
            authorize,
            current,
            until,
            native.drive_io_once(cx, wait),
        )
        .await;
        self.finish_native_io(native, result)?;
        self.check(cx, authorize)?;
        let native = self.native.as_mut().ok_or(Error::Closed)?;
        if native.connection().state() != QuicConnectionState::Established
            || native.connection().inner().streams().len() > self.streams.len()
        {
            self.close();
            return Err(Error::Closed);
        }
        // Retire only the CLOSED epoch, not every record admitted to this route.
        // Waiting records stay counted with their own original deadlines. The
        // live native stream is never cloned or modified to manufacture absence.
        for sender in &mut self.senders {
            if sender.epoch.is_some()
                && !self
                    .pending_writes
                    .iter()
                    .any(|p| p.route == sender.route && p.in_epoch)
                && native
                    .connection()
                    .inner()
                    .streams()
                    .stream(sender.route.stream)
                    .is_ok_and(|live| sender.empty.matches(live))
            {
                let epoch = sender.epoch.take().expect("checked epoch");
                sender.bytes -= epoch.bytes;
                sender.records -= epoch.records;
                sender.until = self
                    .pending_writes
                    .iter()
                    .filter(|p| p.route == sender.route)
                    .map(|p| p.send_by)
                    .min();
            }
        }
        Ok(())
    }
    // A *returned* cancelled/failed I/O turn may transfer custody only into
    // close. Never resume ordinary I/O after that error. The terminal drain must
    // independently prove absence of native payload/retransmissions; uncertain
    // or partially staged work is refused. Dropping an unreturned I/O future
    // still drops its local socket, so no abandoned operation can be resumed.
    fn finish_native_io<T>(
        &mut self,
        native: NativeQuicUdpConnection,
        result: Result<T, Error>,
    ) -> Result<T, Error> {
        self.native = Some(native);
        if result.is_err() {
            self.close();
        }
        result
    }
    /// Stage one small prefix, leaving congestion/loss control with Asupersync.
    /// Large application records are NOT single QUIC frames. No prefix is staged
    /// when native queued work plus flight could fill the protected-packet window.
    fn queue_stream_prefix(&mut self, cx: &Cx, now: u64) -> Result<Option<Priority>, Error> {
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
            return Ok(None);
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
                    || (!pending.in_epoch
                        && self
                            .senders
                            .iter()
                            .any(|s| s.route == pending.route && s.epoch.is_some()))
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
                    selected = Some((index, length, priority));
                    break;
                }
            }
            if selected.is_some() {
                break;
            }
        }
        let Some((index, length, priority)) = selected else {
            return Ok(None);
        };
        let route = self.pending_writes[index].route;
        let sender = self
            .senders
            .iter_mut()
            .find(|s| s.route == route)
            .expect("validated outbound route");
        if sender.epoch.is_none() {
            // Freeze all records currently queued on this stream before the
            // first native prefix. Later sends may queue but cannot join it.
            let mut epoch = SendEpoch {
                bytes: 0,
                records: 0,
            };
            for pending in self.pending_writes.iter_mut().filter(|p| p.route == route) {
                pending.in_epoch = true;
                epoch.bytes += pending.bytes.len();
                epoch.records += 1;
            }
            sender.epoch = Some(epoch);
        }
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
        Ok(Some(priority))
    }
    /// Drain at most 16 borrowed records per turn, alternating streams with
    /// datagrams, one record per lane per round; idle or blocked lanes do not
    /// spend that budget. A blocked handler keeps its complete record (one per route).
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
            if !datagram_admits(route.kind, kind) {
                return Err(Error::WrongRoute);
            }
            validate_record(
                &bytes,
                self.datagram_maximum(route),
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
        // The budget is RECORDS, not lane visits: each lane still yields at
        // most one record per round, and a full round in which no lane yields
        // one ends the turn. Spending a slot per visit let a few idle streams
        // hold a burst of video fragments to two per turn.
        let mut idle = 0;
        while count < TURN_RECORDS && idle < lanes {
            let current = self.check(cx, authorize)?;
            let which = self.cursor % lanes;
            self.cursor = (self.cursor + 1) % lanes;
            let before = count;
            if which == self.inbound.len() {
                count += self.receive_datagram(current, &mut |route, bytes| {
                    if ready(route) {
                        handler(route, bytes)
                    } else {
                        Ok(Disposition::Blocked)
                    }
                })?;
            } else {
                count += self.receive_stream(which, current, cx, ready, handler)?;
            }
            idle = if count == before { idle + 1 } else { 0 };
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
    /// One visit to inbound stream lane `which`: at most one complete record.
    fn receive_stream(
        &mut self,
        which: usize,
        current: u64,
        cx: &Cx,
        ready: &mut impl FnMut(Route) -> bool,
        handler: &mut impl FnMut(Route, &[u8]) -> Result<Disposition, ()>,
    ) -> Result<usize, Error> {
        let mut count = 0;
        let s = &mut self.inbound[which];
        if s.fin
            || !ready(Route::Stream(s.route))
            || self
                .attachments
                .iter()
                .any(|r| r.inbound == s.route.stream && !r.readable())
        {
            return Ok(0);
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
    let bootstrap = streams.iter().any(|r| r.messages == Messages::Negotiation);
    if bootstrap
        && (streams.len() != 2
            || !datagrams.is_empty()
            || streams.iter().filter(|r| r.outbound).count() != 1
            || streams.iter().any(|r| {
                r.messages != Messages::Negotiation
                    || r.binding != 0
                    || r.priority != Priority::Critical
                    || r.maximum > fr_wire::negotiation::MAX_RECORD
            }))
    {
        return Err(Error::InvalidPolicy);
    }
    for (i, r) in streams.iter().enumerate() {
        if (r.binding == 0 && !bootstrap)
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
