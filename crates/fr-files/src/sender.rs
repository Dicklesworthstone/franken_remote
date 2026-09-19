//! Locally selected file -> bounded disk source -> original file channel -> proof.
//!
//! The parent session owns/drives QUIC and supplies live authorization. No path
//! from the peer is opened. Each transfer is offered once, and a lost publication
//! proof is unknown, never an invitation to retry. Service before/after network I/O.
mod source;
use crate::{
    atp,
    receive::{MAX_CHUNK_BYTES, Publication},
};
use asupersync::{
    atp::safety::validate_portable_path_component,
    bytes::BytesMut,
    codec::Decoder,
    cx::Cx,
    net::atp::{
        protocol::{AtpFrameCodec, Frame, FrameType, ProtocolVersion},
        transport_tcp::ReceiveReceipt,
    },
    time::TimerDriverHandle,
};
use fr_transport::quic::{self, ConnectionBinding, QuicRecords, files::FilesChannel};
use fr_wire::{
    WireError,
    files::{self, Body, Disposition, Message, Reason, Role},
};
use source::{Event, Source};
use std::{fs::File, io, time::Duration};

#[derive(Debug, Clone, Copy)]
pub struct Policy {
    pub max_file_bytes: u64,
    pub bytes_per_second: u32,
    pub transfer_lifetime: Duration,
    pub record_lifetime: Duration,
}
impl Default for Policy {
    fn default() -> Self {
        Self {
            max_file_bytes: 4 * 1024 * 1024 * 1024,
            bytes_per_second: 8 * 1024 * 1024,
            transfer_lifetime: Duration::from_mins(30),
            record_lifetime: Duration::from_secs(1),
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Limits,
    Name,
    WrongConnection,
    WrongRole,
    Clock,
    Expired,
    Cancelled,
    Busy,
    Closed,
    Source,
    SourceChanged,
    Spawn,
    Worker,
    Protocol,
    Io(io::ErrorKind),
    Transport(quic::Error),
    Wire(WireError),
}
impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::Io(error.kind())
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "file-send: {self:?}")
    }
}
impl std::error::Error for Error {}

/// The host reports publication; this is not independent filesystem observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    HostPublished {
        bytes: u64,
        publication: Publication,
    },
    HostRefused(Reason),
    /// No completion request was admitted; the host may still own private staging.
    InterruptedBeforePublication(Error),
    /// Completion was admitted or the host reports uncertainty. Never auto-retry.
    PublicationUnknown,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Receipt {
    pub id: u64,
    pub outcome: Outcome,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Idle,
    Preparing,
    AwaitingAcceptance,
    Streaming,
    AwaitingProof,
    Finished,
    Closed,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    pub id: u64,
    /// Bytes queued into the native channel, NOT bytes published at the host.
    pub queued_bytes: u64,
    pub total_bytes: Option<u64>,
}
struct Packet {
    bytes: Vec<u8>,
    deadline: u64,
    cost: u64,
    completing: bool,
}
impl Drop for Packet {
    fn drop(&mut self) {
        self.bytes.fill(0);
    }
}
struct Transfer {
    id: u64,
    source: Source,
    stage: Stage,
    deadline: u64,
    total: Option<u64>,
    root: String,
    queued: u64,
    rate: u32,
    chunk: usize,
    tokens: u128,
    token_time: u64,
    waiting: bool,
    waiting_until: u64,
    ready: Option<Event>,
    packet: Option<Packet>,
    completion_admitted: bool,
    result: Option<Receipt>,
    cleanup: Option<Result<(), Error>>,
}

/// Exclusive borrow prevents independent senders from recreating the channel's
/// sequence domain. A pending result/cleanup must be collected before another
/// file can start. Disk work never runs in `begin`, `service`, or `take_result`.
/// Call `cancel` before abandoning an active sender, then observe cleanup. Drop
/// only requests local source shutdown; it cannot retire a connection it does not
/// own or certify completion of a blocked kernel read.
pub struct Sender<'a> {
    lane: ChannelOwner<'a>,
    connection: ConnectionBinding,
    cx: Cx,
    clock: TimerDriverHandle,
    policy: Policy,
    next: u64,
    transfer: Option<Transfer>,
    closed: bool,
}
// Both entry points use the same transfer state machine and sequence domain.
// Owning the channel avoids a self-referential running session; borrowing keeps
// the existing API and its exclusive-access guarantees intact.
enum ChannelOwner<'a> {
    Borrowed(&'a mut FilesChannel),
    Owned(Box<FilesChannel>),
}
impl std::ops::Deref for ChannelOwner<'_> {
    type Target = FilesChannel;
    fn deref(&self) -> &Self::Target {
        match self {
            Self::Borrowed(channel) => channel,
            Self::Owned(channel) => channel,
        }
    }
}
impl std::ops::DerefMut for ChannelOwner<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        match self {
            Self::Borrowed(channel) => channel,
            Self::Owned(channel) => channel,
        }
    }
}
impl Sender<'static> {
    /// Consume the completed original channel for storage in a running session.
    /// No new attachment, authority, transfer identity or runtime is created.
    /// Call `cancel` before closing the parent and keep this owner until source
    /// cleanup/results are collected. Abandonment conservatively fences the lane.
    pub fn owning(
        cx: Cx,
        q: &QuicRecords,
        lane: FilesChannel,
        policy: Policy,
    ) -> Result<Self, Error> {
        Self::with_channel(cx, q, ChannelOwner::Owned(Box::new(lane)), policy)
    }
}
impl std::fmt::Debug for Sender<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileSender")
            .field("stage", &self.stage())
            .finish_non_exhaustive()
    }
}
impl<'a> Sender<'a> {
    pub fn new(
        cx: Cx,
        q: &QuicRecords,
        lane: &'a mut FilesChannel,
        policy: Policy,
    ) -> Result<Self, Error> {
        Self::with_channel(cx, q, ChannelOwner::Borrowed(lane), policy)
    }
    fn with_channel(
        cx: Cx,
        q: &QuicRecords,
        lane: ChannelOwner<'a>,
        policy: Policy,
    ) -> Result<Self, Error> {
        lane.check(q).map_err(Error::Transport)?;
        if lane.outgoing().sender != Role::Controller {
            return Err(Error::WrongRole);
        }
        if policy.max_file_bytes == 0
            || policy.bytes_per_second == 0
            || policy.transfer_lifetime < Duration::from_micros(1)
            || policy.transfer_lifetime > Duration::from_secs(3600)
            || policy.record_lifetime < Duration::from_micros(1)
            || policy.record_lifetime > Duration::from_secs(1)
            || lane.limits().atp_bytes() < atp::receive::MAX_REPLY_BYTES
        {
            return Err(Error::Limits);
        }
        cx.checkpoint().map_err(|_| Error::Cancelled)?;
        let clock = cx.timer_driver().ok_or(Error::Clock)?;
        Ok(Self {
            lane,
            connection: q.binding(),
            cx,
            clock,
            policy,
            next: 1,
            transfer: None,
            closed: false,
        })
    }
    /// The containing session must reserve this route for this owner, including
    /// while a transfer is pending or its result has not yet been collected.
    pub fn owns_inbound(&self, route: quic::Route) -> bool {
        self.lane.owns_inbound(route)
    }
    pub fn stage(&self) -> Stage {
        self.transfer.as_ref().map_or(
            if self.closed {
                Stage::Closed
            } else {
                Stage::Idle
            },
            |t| t.stage,
        )
    }
    pub fn progress(&self) -> Option<Progress> {
        self.transfer.as_ref().map(|t| Progress {
            id: t.id,
            queued_bytes: t.queued,
            total_bytes: t.total,
        })
    }
    pub fn result(&self) -> Option<Receipt> {
        self.transfer.as_ref().and_then(|t| t.result)
    }
    pub fn cleanup_finished(&self) -> bool {
        self.transfer.as_ref().is_none_or(|t| t.source.finished())
    }
    /// Join only an already-finished original source thread. This never blocks
    /// on a kernel read, clears a publication receipt, or needs a live session.
    /// A panic is a retained cleanup failure, not a fabricated successful drain.
    pub fn try_finish_cleanup(&mut self) -> Option<Result<(), Error>> {
        let Some(t) = &mut self.transfer else {
            return Some(Ok(()));
        };
        if t.cleanup.is_none() {
            t.cleanup = t.source.reap();
        }
        t.cleanup
    }
    /// File is a locally approved descriptor, not a wire path. The name is only
    /// the portable destination basename. Preparation is bounded and asynchronous.
    pub fn begin(&mut self, q: &QuicRecords, file: File, name: &str) -> Result<u64, Error> {
        self.identity(q)?;
        if self.closed {
            return Err(Error::Closed);
        }
        self.lane.check(q).map_err(Error::Transport)?;
        if self.transfer.is_some() {
            return Err(Error::Busy);
        }
        if name.len() > 255
            || name.starts_with(".fr-part-")
            || validate_portable_path_component(name).is_err()
        {
            return Err(Error::Name);
        }
        self.cx.checkpoint().map_err(|_| Error::Cancelled)?;
        let now = self.now();
        let deadline = now
            .checked_add(micros(self.policy.transfer_lifetime)?)
            .ok_or(Error::Clock)?;
        let next = self.next.checked_add(1).ok_or(Error::Limits)?;
        let source = Source::spawn(self.cx.clone(), file, name.into(), self.policy, deadline)?;
        let id = self.next;
        self.next = next;
        self.transfer = Some(Transfer {
            id,
            source,
            stage: Stage::Preparing,
            deadline,
            total: None,
            root: String::new(),
            queued: 0,
            rate: self.policy.bytes_per_second,
            chunk: 0,
            tokens: 0,
            token_time: now,
            waiting: false,
            waiting_until: 0,
            ready: None,
            packet: None,
            completion_admitted: false,
            result: None,
            cleanup: None,
        });
        Ok(id)
    }
    /// Result collection never waits for cleanup. A completed kernel call is not
    /// cancelled retroactively. Published outcomes remain readable after errors.
    pub fn take_result(&mut self) -> Option<Receipt> {
        let t = self.transfer.as_mut()?;
        let result = t.result?;
        if !t.source.finished() {
            return None;
        }
        if t.cleanup.is_none() {
            t.cleanup = t.source.reap();
        }
        self.transfer = None;
        Some(result)
    }
    /// Local file-only cancellation is out of band from bulk queue capacity.
    /// The caller can keep servicing the original desktop connection normally.
    pub fn cancel(&mut self, q: &mut QuicRecords) -> Result<(), Error> {
        self.identity(q)?;
        self.fail(Error::Cancelled);
        self.retire(q)
    }
    pub fn service(
        &mut self,
        q: &mut QuicRecords,
        mut authorize: impl FnMut() -> bool,
    ) -> Result<Stage, Error> {
        self.identity(q)?;
        if self.closed {
            return Ok(self.stage());
        }
        let result = self.turn(q, &mut authorize);
        if let Err(error) = result {
            self.fail(error);
            let _ = self.retire(q);
            return Err(error);
        }
        Ok(self.stage())
    }
    fn turn(
        &mut self,
        q: &mut QuicRecords,
        authorize: &mut impl FnMut() -> bool,
    ) -> Result<(), Error> {
        self.cx.checkpoint().map_err(|_| Error::Cancelled)?;
        self.lane.check(q).map_err(Error::Transport)?;
        let now = self.now();
        if let Some(t) = &self.transfer
            && t.result.is_none()
            && (now >= t.deadline || (t.waiting && now >= t.waiting_until))
        {
            return Err(Error::Expired);
        }
        let limits = self.lane.limits();
        let incoming = self.lane.incoming();
        let current = &mut self.transfer;
        let mut refusal = None;
        self.lane
            .dispatch(q, &self.cx, &mut *authorize, |bytes| {
                let result = current
                    .as_mut()
                    .ok_or(Error::Protocol)
                    .and_then(|t| t.reply(bytes, incoming, limits, now));
                if let Err(error) = result {
                    refusal = Some(error);
                }
                Ok(quic::Disposition::Consumed)
            })
            .map_err(Error::Transport)?;
        if let Some(error) = refusal {
            return Err(error);
        }
        let Some(t) = self.transfer.as_mut() else {
            return Ok(());
        };
        if let Some(receipt) = t.result {
            t.source.stop();
            if !matches!(receipt.outcome, Outcome::HostPublished { .. }) {
                self.retire(q)?;
            }
            return Ok(());
        }
        if now < t.token_time {
            return Err(Error::Clock);
        }
        t.tokens = (t.tokens + u128::from(now - t.token_time) * u128::from(t.rate))
            .min((MAX_CHUNK_BYTES as u128 + 128) * 1_000_000);
        t.token_time = now;
        if t.waiting {
            if !self.lane.send_drained(q).map_err(Error::Transport)? {
                return Ok(());
            }
            t.waiting = false;
        }
        t.prepare(
            self.lane.outgoing(),
            limits,
            now,
            self.policy.record_lifetime,
        )?;
        if let Some(p) = t.packet.as_ref() {
            if now >= p.deadline {
                return Err(Error::Expired);
            }
            match self
                .lane
                .send(q, &self.cx, &p.bytes, p.deadline, &mut *authorize)
            {
                Ok(()) => {
                    if p.cost == 0 {
                        t.stage = Stage::AwaitingAcceptance;
                    } else {
                        t.tokens -= u128::from(p.cost) * 1_000_000;
                        if p.completing {
                            t.completion_admitted = true;
                            t.stage = Stage::AwaitingProof;
                        } else {
                            t.queued += p.cost - 128;
                        }
                    }
                    t.waiting_until = p.deadline;
                    t.packet = None;
                    t.waiting = true;
                }
                Err(quic::Error::Backpressure) => return Ok(()),
                Err(error) => return Err(Error::Transport(error)),
            }
        }
        if t.stage == Stage::Streaming
            && !t.waiting
            && t.ready.is_none()
            && t.packet.is_none()
            && !t.source.pending()
        {
            t.source.request(t.chunk)?;
        }
        Ok(())
    }
    fn fail(&mut self, error: Error) {
        self.closed = true;
        if let Some(t) = self.transfer.as_mut() {
            t.source.stop();
            t.ready = None;
            t.packet = None;
            if t.result.is_none() {
                t.result = Some(Receipt {
                    id: t.id,
                    outcome: if t.completion_admitted {
                        Outcome::PublicationUnknown
                    } else {
                        Outcome::InterruptedBeforePublication(error)
                    },
                });
            }
            t.stage = Stage::Finished;
        }
    }
    fn retire(&mut self, q: &mut QuicRecords) -> Result<(), Error> {
        self.closed = true;
        if q.is_closed() {
            return Ok(());
        }
        self.lane.retire(q, &self.cx).map_err(Error::Transport)
    }
    fn identity(&self, q: &QuicRecords) -> Result<(), Error> {
        if q.is_bound_to(&self.connection) {
            Ok(())
        } else {
            Err(Error::WrongConnection)
        }
    }
    fn now(&self) -> u64 {
        self.clock.now().as_nanos() / 1000
    }
}
impl Drop for Sender<'_> {
    fn drop(&mut self) {
        if let Some(t) = &self.transfer {
            t.source.stop();
        }
    }
}
fn micros(d: Duration) -> Result<u64, Error> {
    u64::try_from(d.as_micros()).map_err(|_| Error::Limits)
}
#[allow(clippy::too_many_arguments)]
fn packet(
    id: u64,
    body: Body<'_>,
    context: files::Context,
    limits: files::Limits,
    now: u64,
    until: u64,
    lifetime: Duration,
    cost: u64,
    completing: bool,
) -> Result<Packet, Error> {
    let mut bytes = vec![0; limits.record_bytes()];
    let n =
        files::encode(Message { id, body }, context, limits, &mut bytes).map_err(Error::Wire)?;
    bytes.truncate(n);
    Ok(Packet {
        bytes,
        deadline: now
            .checked_add(micros(lifetime)?)
            .ok_or(Error::Clock)?
            .min(until),
        cost,
        completing,
    })
}
impl Transfer {
    fn prepare(
        &mut self,
        context: files::Context,
        limits: files::Limits,
        now: u64,
        lifetime: Duration,
    ) -> Result<(), Error> {
        if self.ready.is_none() && self.packet.is_none() {
            self.ready = self.source.poll()?;
        }
        if let Some(Event::Prepared(_)) = self.ready {
            let Some(Event::Prepared(manifest)) = self.ready.take() else {
                unreachable!()
            };
            if self.stage != Stage::Preparing {
                return Err(Error::Protocol);
            }
            self.total = Some(manifest.total_bytes);
            self.root.clone_from(&manifest.merkle_root_hex);
            let payload = serde_json::to_vec(&manifest).map_err(|_| Error::Protocol)?;
            if payload.len() > atp::receive::MAX_MANIFEST_BYTES {
                return Err(Error::Limits);
            }
            let frame = Frame::new(ProtocolVersion::CURRENT, FrameType::ObjectManifest, payload)
                .and_then(|f| f.to_wire_bytes())
                .map_err(|_| Error::Protocol)?;
            self.packet = Some(packet(
                self.id,
                Body::Offer {
                    profile: files::ATP_PORTABLE_FULL,
                    atp: &frame,
                },
                context,
                limits,
                now,
                self.deadline,
                lifetime,
                0,
                false,
            )?);
        }
        if self.packet.is_none() && self.stage == Stage::Streaming {
            let cost = match &self.ready {
                Some(Event::Chunk { bytes, .. }) => bytes.len() as u64 + 128,
                Some(Event::End) => 128,
                _ => 0,
            };
            if cost != 0 && self.tokens >= u128::from(cost) * 1_000_000 {
                let event = self.ready.take().ok_or(Error::Protocol)?;
                let (frame, completing) = match event {
                    Event::Chunk { offset, bytes } => {
                        if offset != self.queued || bytes.len() > self.chunk {
                            return Err(Error::Protocol);
                        }
                        (
                            atp::encode_data(offset, &bytes).map_err(|_| Error::Protocol)?,
                            false,
                        )
                    }
                    Event::End => {
                        if self.total != Some(self.queued) {
                            return Err(Error::SourceChanged);
                        }
                        (atp::encode_complete().map_err(|_| Error::Protocol)?, true)
                    }
                    Event::Prepared(_) => return Err(Error::Protocol),
                };
                self.packet = Some(packet(
                    self.id,
                    Body::Chunk { atp: &frame },
                    context,
                    limits,
                    now,
                    self.deadline,
                    lifetime,
                    cost,
                    completing,
                )?);
            }
        }
        Ok(())
    }

    fn reply(
        &mut self,
        bytes: &[u8],
        context: files::Context,
        limits: files::Limits,
        now: u64,
    ) -> Result<(), Error> {
        let message = files::decode(bytes, context, limits).map_err(Error::Wire)?;
        if message.id != self.id || self.result.is_some() {
            return Err(Error::Protocol);
        }
        match message.body {
            Body::Accept {
                size,
                bytes_per_second,
                chunk_bytes,
                atp,
                ..
            } => {
                if self.stage != Stage::AwaitingAcceptance || self.total != Some(size) {
                    return Err(Error::Protocol);
                }
                let frame = frame(atp, FrameType::ObjectRequest)?;
                let v: serde_json::Value =
                    serde_json::from_slice(&frame.payload).map_err(|_| Error::Protocol)?;
                if v["mode"] != "full_object"
                    || v["sender_merkle_root_hex"] != self.root
                    || v["missing_bytes"].as_u64() != Some(size)
                    || v["shared_chunks"].as_u64() != Some(0)
                    || v["stale_chunks"].as_u64() != Some(0)
                    || !v["missing_chunks"].as_array().is_some_and(Vec::is_empty)
                {
                    return Err(Error::Protocol);
                }
                self.rate = self.rate.min(bytes_per_second);
                self.chunk = (chunk_bytes as usize).min(MAX_CHUNK_BYTES);
                self.tokens = 0;
                self.token_time = now;
                self.stage = Stage::Streaming;
            }
            Body::Complete {
                disposition,
                reason,
                published_bytes,
                atp,
            } => {
                if self.stage == Stage::Preparing {
                    return Err(Error::Protocol);
                }
                let outcome = match disposition {
                    Disposition::Refused => Outcome::HostRefused(reason),
                    Disposition::UnknownEffect => Outcome::PublicationUnknown,
                    Disposition::PublishedDurable | Disposition::PublishedDurabilityUnknown => {
                        if self.stage != Stage::AwaitingProof || self.total != Some(published_bytes)
                        {
                            return Err(Error::Protocol);
                        }
                        let frame = frame(atp, FrameType::Proof)?;
                        let p: ReceiveReceipt =
                            serde_json::from_slice(&frame.payload).map_err(|_| Error::Protocol)?;
                        if !p.committed
                            || !p.sha_ok
                            || !p.merkle_ok
                            || p.bytes_received != published_bytes
                            || p.files != 1
                            || p.reason.is_some()
                            || !p.committed_paths.is_empty()
                            || p.symbols_accepted != 0
                            || p.feedback_rounds != 0
                            || p.decode_count != 0
                            || p.decode_micros != 0
                        {
                            return Err(Error::Protocol);
                        }
                        Outcome::HostPublished {
                            bytes: published_bytes,
                            publication: if disposition == Disposition::PublishedDurable {
                                Publication::Durable
                            } else {
                                Publication::DurabilityUnknown
                            },
                        }
                    }
                };
                self.result = Some(Receipt {
                    id: self.id,
                    outcome,
                });
                self.stage = Stage::Finished;
            }
            Body::Cancel(_) => return Err(Error::Cancelled),
            _ => return Err(Error::Protocol),
        }
        Ok(())
    }
}
fn frame(bytes: &[u8], kind: FrameType) -> Result<Frame, Error> {
    if bytes.len() > atp::receive::MAX_REPLY_BYTES {
        return Err(Error::Protocol);
    }
    let mut buffer = BytesMut::from(bytes);
    let frame = AtpFrameCodec::with_max_frame_size(atp::receive::MAX_REPLY_BYTES as u64)
        .decode(&mut buffer)
        .map_err(|_| Error::Protocol)?
        .ok_or(Error::Protocol)?;
    if !buffer.is_empty()
        || frame.frame_type() != kind
        || frame.version() != ProtocolVersion::CURRENT
        || !frame.header.extensions.is_empty()
    {
        return Err(Error::Protocol);
    }
    Ok(frame)
}
