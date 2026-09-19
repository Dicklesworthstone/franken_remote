//! Bounded membership and fair native sends for one already-admitted shared source.
//! Native capture, session renewal, connection driving and input cleanup remain
//! with their original owners. This set owns senders, never viewers' authority
//! grants or the encoder lifetime. No connection is borrowed across an await.
use super::{Error as SendError, QuicEgress};
use crate::{
    media::{CaptureSource, SharedCaptureUpdate, decoder_startup},
    media_egress::{Lane, Progress},
};
use asupersync::cx::Cx;
use fr_core::{ids::RemoteSessionId, time::HostInstant};
use fr_transport::quic::{ConnectionBinding, QuicRecords, Route};
use std::sync::Arc;

pub const MAX_MEMBERS: usize = 8;
pub const MAX_ATTEMPTS_PER_TURN: u8 = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    InvalidPolicy,
    Full,
    MissingSender,
    Empty,
    WrongSource,
    DuplicateSession,
    StaleMember,
    WrongConnections,
    SequenceExhausted,
    Closed,
    Startup(decoder_startup::Error),
    Send(SendError),
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Error {}
/// Local routing handle, not a permission or a peer-supplied identifier. Slot
/// reuse changes its serial; equal numbers from another set cannot select it.
#[derive(Clone)]
pub struct MemberId {
    owner: Arc<()>,
    slot: usize,
    serial: u64,
}
impl PartialEq for MemberId {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.owner, &other.owner)
            && self.slot == other.slot
            && self.serial == other.serial
    }
}
impl Eq for MemberId {}
impl std::fmt::Debug for MemberId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemberId")
            .field("serial", &self.serial)
            .finish_non_exhaustive()
    }
}

/// A completed native decoder handshake joined to its original source/sender.
/// Dropping ownership fences readiness before freeing retained packets. Detach
/// explicitly to transfer this SAME sender into recovery, never a new cache.
pub struct ReadySender {
    sender: Option<QuicEgress>,
    source: Arc<()>,
    connection: ConnectionBinding,
    session: RemoteSessionId,
}
impl std::fmt::Debug for ReadySender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReadySender")
            .field("present", &self.sender.is_some())
            .finish_non_exhaustive()
    }
}
impl ReadySender {
    pub fn new(
        startup: decoder_startup::Host,
        source: &CaptureSource,
        mut sender: QuicEgress,
        connection: &QuicRecords,
    ) -> Result<Self, Error> {
        let result = (|| {
            let (control, view) = startup.finish_stream(connection).map_err(Error::Startup)?;
            sender
                .join_stream(connection, source, &control, view)
                .map_err(Error::Send)?;
            let identity = sender
                .egress
                .stream_subscription()
                .and_then(crate::media::Subscription::shared_identity)
                .map_err(|e| Error::Send(SendError::Media(e)))?;
            Ok((identity, view.parent.remote_session))
        })();
        match result {
            Ok((source, session)) => Ok(Self {
                sender: Some(sender),
                source,
                connection: connection.binding(),
                session,
            }),
            Err(error) => {
                sender.egress.close();
                Err(error)
            }
        }
    }
    /// Transfer the original sender to an explicit session/recovery owner. This
    /// does not grant authority, renew a deadline, or reset repair/recovery credit.
    pub fn into_sender(mut self) -> QuicEgress {
        self.sender.take().expect("owned sender")
    }
    fn sender(&mut self) -> &mut QuicEgress {
        self.sender.as_mut().expect("owned sender")
    }
    fn retire(&mut self) {
        if let Some(sender) = &mut self.sender {
            sender.egress.close();
        }
    }
}
impl Drop for ReadySender {
    fn drop(&mut self) {
        self.retire();
    }
}

struct Member {
    serial: u64,
    ready: ReadySender,
    next_lane: Lane,
    failure: Option<SendError>,
}
impl Member {
    fn fail(&mut self, error: SendError) {
        // Keep the first bounded reason until the parent removes this member.
        if self.failure.is_none() {
            self.failure = Some(error);
        }
        self.ready.retire();
    }
}
#[derive(Debug, Clone)]
pub struct MemberProgress {
    pub member: MemberId,
    /// Actual transport admissions, not delivery/decode/presentation counts.
    pub admitted: u8,
    pub pending: bool,
    pub failure: Option<SendError>,
}
/// Fixed-space report, including members without send credit. Failed entries
/// remain present until explicitly removed, so failures cannot be silently lost.
#[derive(Debug)]
pub struct Report {
    entries: [Option<MemberProgress>; MAX_MEMBERS],
    attempts: u8,
}
impl Report {
    pub fn members(&self) -> impl Iterator<Item = &MemberProgress> {
        self.entries.iter().flatten()
    }
    pub const fn attempts(&self) -> u8 {
        self.attempts
    }
}

/// At most eight existing senders with one stable source identity. A turn visits
/// each ready member in round-robin order, bounded by ATTEMPTS rather than only
/// successes. A blocked member is tried once per turn and cannot busy-spin or
/// prevent later viewers' sends. Originals and repairs alternate per member;
/// an already prepared packet always retains precedence in the canonical egress.
pub struct SendSet {
    owner: Arc<()>,
    source: Option<Arc<()>>,
    slots: [Option<Member>; MAX_MEMBERS],
    maximum: usize,
    serial: u64,
    cursor: usize,
    closed: bool,
}
impl std::fmt::Debug for SendSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SendSet")
            .field("members", &self.len())
            .field("closed", &self.closed)
            .finish_non_exhaustive()
    }
}
impl SendSet {
    pub fn new(maximum: usize) -> Result<Self, Error> {
        if !(1..=MAX_MEMBERS).contains(&maximum) {
            return Err(Error::InvalidPolicy);
        }
        Ok(Self {
            owner: Arc::new(()),
            source: None,
            slots: core::array::from_fn(|_| None),
            maximum,
            serial: 0,
            cursor: 0,
            closed: false,
        })
    }
    pub fn len(&self) -> usize {
        self.slots.iter().flatten().count()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn active(&self) -> usize {
        self.slots
            .iter()
            .flatten()
            .filter(|m| m.failure.is_none())
            .count()
    }
    /// Consume the pending owner only on successful admission. On refusal it
    /// remains INTACT: no heap allocation to return a large sender, no lost
    /// ownership on a local capacity/identity/duplicate-session refusal.
    pub fn insert(&mut self, pending: &mut Option<ReadySender>) -> Result<MemberId, Error> {
        let sender = pending.as_ref().ok_or(Error::MissingSender)?;
        let reason = if self.closed || sender.sender.as_ref().is_none_or(QuicEgress::is_closed) {
            Some(Error::Closed)
        } else if self
            .source
            .as_ref()
            .is_some_and(|s| !Arc::ptr_eq(s, &sender.source))
        {
            Some(Error::WrongSource)
        } else if self
            .slots
            .iter()
            .flatten()
            .any(|m| m.ready.session == sender.session)
        {
            Some(Error::DuplicateSession)
        } else if self.len() >= self.maximum {
            Some(Error::Full)
        } else if self.serial == u64::MAX {
            Some(Error::SequenceExhausted)
        } else {
            None
        };
        if let Some(error) = reason {
            return Err(error);
        }
        let slot = self
            .slots
            .iter()
            .position(Option::is_none)
            .expect("bounded free slot");
        self.serial += 1;
        if self.source.is_none() {
            self.source = Some(sender.source.clone());
        }
        let sender = pending.take().expect("preflight kept the original sender");
        self.slots[slot] = Some(Member {
            serial: self.serial,
            ready: sender,
            next_lane: Lane::Original,
            failure: None,
        });
        Ok(self.id(slot))
    }
    fn id(&self, slot: usize) -> MemberId {
        MemberId {
            owner: self.owner.clone(),
            slot,
            serial: self.slots[slot].as_ref().expect("present").serial,
        }
    }
    fn index(&self, id: &MemberId) -> Result<usize, Error> {
        if !Arc::ptr_eq(&self.owner, &id.owner)
            || self
                .slots
                .get(id.slot)
                .and_then(Option::as_ref)
                .is_none_or(|m| m.serial != id.serial)
        {
            return Err(Error::StaleMember);
        }
        Ok(id.slot)
    }
    /// No generation-persistent sender policy is reset when ownership moves.
    pub fn detach(&mut self, id: &MemberId) -> Result<ReadySender, Error> {
        let index = self.index(id)?;
        Ok(self.slots[index].take().expect("checked").ready)
    }
    /// Readiness/input is fenced before cached references are released. The
    /// original parent performs native release cleanup and retires its connection.
    pub fn retire(&mut self, id: &MemberId) -> Result<(), Error> {
        drop(self.detach(id)?);
        Ok(())
    }
    pub fn close(&mut self) {
        for m in self.slots.iter_mut().flatten() {
            m.fail(SendError::Closed);
        }
        self.closed = true;
    }
    pub fn next_deadline(&self) -> Option<HostInstant> {
        self.slots
            .iter()
            .flatten()
            .filter(|m| m.failure.is_none())
            .filter_map(|m| m.ready.sender.as_ref().and_then(QuicEgress::next_deadline))
            .min()
    }
    fn report(&self) -> Report {
        Report {
            entries: core::array::from_fn(|slot| {
                self.slots[slot].as_ref().map(|m| MemberProgress {
                    member: self.id(slot),
                    admitted: 0,
                    pending: m
                        .ready
                        .sender
                        .as_ref()
                        .is_some_and(|s| s.pending().is_some()),
                    failure: m.failure,
                })
            }),
            attempts: 0,
        }
    }
    /// Service all retention and observation deadlines even without network
    /// credit, traffic, or another capture. No deadline is extended by polling.
    pub fn tick(&mut self) -> Report {
        for m in self.slots.iter_mut().flatten() {
            if m.failure.is_none()
                && let Err(e) = m.ready.sender().tick()
            {
                m.fail(e);
            }
        }
        self.report()
    }
    /// One already-produced source-bound output, admitted independently by every
    /// surviving member. Foreign output refuses before any member is mutated.
    /// No new capture or late-join IDR is implicitly requested by this operation.
    pub fn publish(&mut self, update: &SharedCaptureUpdate) -> Result<Report, Error> {
        if self.closed {
            return Err(Error::Closed);
        }
        let source = self.source.as_ref().ok_or(Error::Empty)?;
        if !update.belongs_to_shared_source(source) {
            return Err(Error::WrongSource);
        }
        if self.active() == 0 {
            return Err(Error::Empty);
        }
        for m in self.slots.iter_mut().flatten() {
            if m.failure.is_none()
                && let Err(e) = m.ready.sender().enqueue_shared_capture(update)
            {
                m.fail(e);
            }
        }
        Ok(self.report())
    }
    /// Every retained member must appear EXACTLY once with its original native
    /// connection. Preflight the entire mapping before clocks, media or QUIC
    /// queues are mutated. Session control/renewal must be driven independently.
    pub fn transmit(
        &mut self,
        cx: &Cx,
        peers: &mut [(&MemberId, &mut QuicRecords)],
        maximum_attempts: u8,
    ) -> Result<Report, Error> {
        if self.closed {
            return Err(Error::Closed);
        }
        if !(1..=MAX_ATTEMPTS_PER_TURN).contains(&maximum_attempts) {
            return Err(Error::InvalidPolicy);
        }
        if peers.len() != self.len() {
            return Err(Error::WrongConnections);
        }
        let mut mapping = [None; MAX_MEMBERS];
        for (offset, (id, q)) in peers.iter().enumerate() {
            let slot = self.index(id)?;
            let member = self.slots[slot].as_ref().expect("checked");
            if mapping[slot].is_some() || !q.is_bound_to(&member.ready.connection) {
                return Err(Error::WrongConnections);
            }
            mapping[slot] = Some(offset);
        }
        if cx.checkpoint().is_err() {
            self.close();
            return Err(Error::Closed);
        }
        let mut report = self.tick();
        let mut skipped = core::array::from_fn::<_, MAX_MEMBERS, _>(|i| {
            self.slots[i].as_ref().is_none_or(|m| m.failure.is_some())
        });
        while report.attempts < maximum_attempts {
            let Some(slot) = (0..MAX_MEMBERS)
                .map(|step| (self.cursor + step) % MAX_MEMBERS)
                .find(|&i| !skipped[i])
            else {
                break;
            };
            self.cursor = (slot + 1) % MAX_MEMBERS;
            report.attempts += 1;
            let member = self.slots[slot].as_mut().expect("selected");
            let q = &mut *peers[mapping[slot].expect("preflight")].1;
            let lane = member.next_lane;
            member.next_lane = other_lane(lane);
            // At most two bounded canonical operations per visit. A pending
            // original is never bypassed by a repair, even when lanes alternate.
            let mut result = member.ready.sender().transmit(cx, q, lane);
            if matches!(result, Ok(Progress::Idle)) {
                result = member.ready.sender().transmit(cx, q, other_lane(lane));
            }
            let entry = report.entries[slot].as_mut().expect("selected report");
            match result {
                Ok(Progress::Accepted(_)) => {
                    entry.admitted += 1;
                    entry.pending = false;
                }
                Ok(Progress::Pending(_)) => {
                    skipped[slot] = true;
                    entry.pending = true;
                }
                Ok(Progress::Idle) => {
                    skipped[slot] = true;
                    entry.pending = false;
                }
                Err(error) => {
                    member.fail(error);
                    skipped[slot] = true;
                    entry.pending = false;
                    entry.failure = member.failure;
                }
            }
        }
        Ok(report)
    }
    /// Route bounded selective repair to the original sender; it consumes the
    /// existing repair-rate allowance. Wrong handles/connections are non-mutating.
    pub fn repair(
        &mut self,
        id: &MemberId,
        q: &QuicRecords,
        route: Route,
        bytes: &[u8],
    ) -> Result<super::RepairAdmission, Error> {
        let slot = self.index(id)?;
        let m = self.slots[slot].as_mut().expect("checked");
        if !q.is_bound_to(&m.ready.connection) {
            return Err(Error::WrongConnections);
        }
        if m.failure.is_some() {
            return Err(Error::Closed);
        }
        match m.ready.sender().repair_on(q, route, bytes) {
            Ok(result) => Ok(result),
            Err(e) => {
                m.fail(e);
                Err(Error::Send(e))
            }
        }
    }
}
impl Drop for SendSet {
    fn drop(&mut self) {
        self.close();
    }
}
fn other_lane(lane: Lane) -> Lane {
    match lane {
        Lane::Original => Lane::Repair,
        Lane::Repair => Lane::Original,
    }
}
