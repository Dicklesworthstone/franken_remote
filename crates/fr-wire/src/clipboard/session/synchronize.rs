//! Automatic bidirectional clipboard work on one admitted native worker.
//!
//! No polling of text while idle, runtime, thread, input grant or transport is
//! manufactured here. The caller schedules bounded turns and supplies a qualified
//! host-clock projection, item randomness and the separately attached record lane.
use super::{
    CancelReason, ChannelSession, ClipboardSink, ClipboardSwitch, Error, HostInstant, Offer, Pump,
    Receipt, RecordSink, SessionError, Stamp,
};
use core::fmt;

/// Native metadata is descriptive, not bearer authority. Backends only report
/// their own exact publication stamp, never deduplicate by text equality.
#[derive(Debug, Clone, Copy)]
pub struct NativeChange {
    pub revision: u64,
    pub has_selection: bool,
    pub origin: Option<Stamp>,
}
#[derive(Debug, Clone, Copy)]
pub struct NativeChanges {
    pub latest: Option<NativeChange>,
    /// False when the fixed event budget was exhausted. Old sends are retired
    /// immediately, but reads/publications wait for a current, settled selection.
    pub settled: bool,
}
/// A complete, validated, zero-on-drop native buffer; never a partial selection.
pub trait NativeText {
    fn text(&self) -> &str;
    fn origin(&self) -> Option<Stamp>;
}
/// Thread-confined native owner with bounded turns. All calls happen outside the
/// input authority lock. A successful/uncertain publication must cancel its
/// pending native read. Suspend/close erase private bytes without overwriting
/// another application's newer selection. Errors must not contain native text,
/// paths, display names, window IDs or untrusted library strings.
pub trait NativeClipboard: ClipboardSink {
    type Text: NativeText;
    type Error: fmt::Debug;
    /// Start a fresh subscription and return its bootstrap revision.
    fn watch(&mut self) -> Result<u64, Self::Error>;
    fn changes(&mut self) -> Result<NativeChanges, Self::Error>;
    fn revision(&self) -> u64;
    /// Prepare without publishing, then service bounded native change metadata
    /// and reject unless `revision` still describes the current selection.
    /// Keep any discovered change pending for the next `changes` call. The core
    /// rechecks authority and expiry AFTER this potentially blocking operation.
    fn prepare_for_revision(
        &mut self,
        text: &str,
        stamp: Stamp,
        revision: u64,
    ) -> Result<(), fr_core::clipboard::PlatformError>;
    fn begin_read(&mut self) -> Result<(), Self::Error>;
    fn poll_read(&mut self) -> Result<Option<Self::Text>, Self::Error>;
    fn cancel_read(&mut self);
    fn suspend(&mut self) -> Result<(), Self::Error>;
    fn close(&mut self);
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdentifierFailure;
#[derive(Debug, PartialEq, Eq)]
pub enum SyncError<E> {
    Session(SessionError),
    Native(E),
    Identifier,
    NativeRevision,
}
impl<E> From<SessionError> for SyncError<E> {
    fn from(error: SessionError) -> Self {
        Self::Session(error)
    }
}
#[derive(Debug)]
pub enum ReadProgress<E> {
    Idle,
    Pending,
    Queued(Stamp),
    /// A failed selection is consumed. Only a genuinely new native revision can
    /// start another read; silence never causes an automatic retry.
    Refused(E),
    Expired,
    Superseded,
    Suspended,
}
#[derive(Debug)]
pub struct Progress<E> {
    pub read: ReadProgress<E>,
    pub send: Pump,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Received {
    /// Nothing consumed; retain at most one bounded record and try a later turn.
    Deferred,
    Consumed(Option<Receipt>),
    /// Consumed refusal, not permission to replay with new credentials.
    Refused(SessionError),
}
struct Reading {
    id: u128,
    revision: u64,
    deadline: HostInstant,
    generation: u64,
}

/// Unique owner of both channel directions and the native subscription. There
/// is deliberately no mutable escape to either owner and no reconnect/rebind.
/// A new controller requires a new admitted channel and a new synchronizer.
pub struct Synchronizer<N: NativeClipboard> {
    channel: ChannelSession,
    native: N,
    reading: Option<Reading>,
    candidate: Option<NativeChange>,
    seen_revision: u64,
    generation: u64,
    watching: bool,
    started: bool,
    bootstrap_floor: u64,
    closed: bool,
}
impl<N: NativeClipboard> fmt::Debug for Synchronizer<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClipboardSynchronizer")
            .field("closed", &self.is_closed())
            .field("reading", &self.reading.is_some())
            .field("watching", &self.watching)
            .finish_non_exhaustive()
    }
}
struct Turn<'a, N: NativeClipboard> {
    owner: &'a mut Synchronizer<N>,
    completed: bool,
}
impl<N: NativeClipboard> Drop for Turn<'_, N> {
    fn drop(&mut self) {
        if !self.completed {
            self.owner.close();
        }
    }
}
impl<N: NativeClipboard> Synchronizer<N> {
    /// Takes ownership but does no native observation until the first admitted
    /// turn. `channel` must already have its separate clipboard grant/attachment.
    pub fn new(channel: ChannelSession, native: N) -> Self {
        let generation = channel.observation_generation;
        Self {
            channel,
            native,
            reading: None,
            candidate: None,
            seen_revision: 0,
            generation,
            watching: false,
            started: false,
            bootstrap_floor: 0,
            closed: false,
        }
    }
    pub fn local_switch(&self) -> ClipboardSwitch {
        self.channel.local_switch()
    }
    pub fn peer_switch(&self) -> ClipboardSwitch {
        self.channel.peer_switch()
    }
    pub const fn is_closed(&self) -> bool {
        self.closed || self.channel.is_closed()
    }
    pub const fn is_reading(&self) -> bool {
        self.reading.is_some()
    }
    pub fn retained_channel_bytes(&self) -> usize {
        self.channel.retained_bytes()
    }
    /// Fence the channel before native teardown. Does not revoke input/media.
    /// The caller must also fence already accepted transport records.
    pub fn close(&mut self) {
        self.channel.close();
        self.reading = None;
        self.candidate = None;
        self.watching = false;
        if !self.closed {
            self.closed = true;
            self.native.close();
        }
    }
    fn retire_read(&mut self) {
        self.reading = None;
        self.native.cancel_read();
    }
    fn suspend(&mut self) -> Result<(), SyncError<N::Error>> {
        self.reading = None;
        self.candidate = None;
        self.generation = self.channel.observation_generation;
        if self.watching {
            self.watching = false;
            self.native.suspend().map_err(SyncError::Native)?;
        }
        Ok(())
    }
    fn admit(&mut self, now: HostInstant) -> Result<bool, SyncError<N::Error>> {
        if self.is_closed() {
            return Err(SessionError::Clipboard(Error::Closed).into());
        }
        let state = self.channel.maintain(now)?;
        if !state.enabled || self.generation != self.channel.observation_generation {
            self.suspend()?;
            return Ok(false);
        }
        Ok(true)
    }
    fn native_changes(
        &mut self,
        clock: &mut impl FnMut() -> HostInstant,
    ) -> Result<bool, SyncError<N::Error>> {
        if !self.admit(clock())? {
            return Ok(false);
        }
        if !self.watching {
            let bootstrap = self.native.watch().map_err(SyncError::Native)?;
            self.watching = true;
            // Re-enable never automatically replays the interrupted selection.
            // Initial attachment observes the current selection once; later
            // subscriptions wait for a genuine new copy after their bootstrap.
            self.bootstrap_floor = if self.started { bootstrap } else { 0 };
            self.started = true;
            if !self.admit(clock())? {
                return Ok(false);
            }
        }
        let changes = self.native.changes().map_err(SyncError::Native)?;
        let now = clock();
        if !self.admit(now)? {
            return Ok(false);
        }
        if self.native.revision() < self.seen_revision {
            return Err(SyncError::NativeRevision);
        }
        if let Some(change) = changes.latest {
            if change.revision == 0
                || change.revision > self.native.revision()
                || change.revision < self.seen_revision
            {
                return Err(SyncError::NativeRevision);
            }
            if change.revision > self.seen_revision {
                self.seen_revision = change.revision;
                self.retire_read();
                self.candidate = None;
                if change.revision > self.bootstrap_floor {
                    let genuine = self
                        .channel
                        .receiver
                        .local_change(change.origin, now)
                        .map_err(|error| self.channel.clipboard_error(error))?;
                    if genuine {
                        self.channel.discard_pending(CancelReason::Superseded);
                        if change.has_selection {
                            self.candidate = Some(change);
                        }
                    }
                }
            }
        }
        Ok(changes.settled)
    }
    /// One native change turn, at most one native read step, and at most one
    /// outbound record. Call during silence as well as traffic. A native read
    /// failure is reported once; a later copy can work without reopening input.
    pub fn poll(
        &mut self,
        scratch: &mut [u8],
        sink: &mut impl RecordSink,
        mut clock: impl FnMut() -> HostInstant,
        mut new_id: impl FnMut() -> Result<u128, IdentifierFailure>,
    ) -> Result<Progress<N::Error>, SyncError<N::Error>> {
        let mut turn = Turn {
            owner: self,
            completed: false,
        };
        let size = scratch
            .len()
            .min(turn.owner.channel.limits.max_control_message_bytes() as usize);
        let scratch = super::Scratch(&mut scratch[..size]);
        let progress = match turn
            .owner
            .poll_inner(scratch.0, sink, &mut clock, &mut new_id)
        {
            Ok(progress) => progress,
            Err(SyncError::Session(SessionError::Clipboard(Error::Disabled)))
                if !turn.owner.is_closed() =>
            {
                turn.owner.suspend()?;
                Progress {
                    read: ReadProgress::Suspended,
                    send: Pump::Deferred,
                }
            }
            Err(error) => return Err(error),
        };
        turn.completed = true;
        Ok(progress)
    }
    fn poll_inner(
        &mut self,
        scratch: &mut [u8],
        sink: &mut impl RecordSink,
        clock: &mut impl FnMut() -> HostInstant,
        new_id: &mut impl FnMut() -> Result<u128, IdentifierFailure>,
    ) -> Result<Progress<N::Error>, SyncError<N::Error>> {
        let settled = self.native_changes(clock)?;
        if !self.watching {
            // Release-only cancellation may still be admitted while disabled.
            let send = self.channel.pump(scratch, sink, &mut *clock)?;
            return Ok(Progress {
                read: ReadProgress::Suspended,
                send,
            });
        }
        if !settled {
            // Do not send old text or accept a new snapshot from an event backlog.
            return Ok(Progress {
                read: ReadProgress::Pending,
                send: Pump::Deferred,
            });
        }
        let read = self.read_step(clock, new_id)?;
        // poll_read may itself service ownership changes. Retire/fence before
        // any stale bytes escape; the next bounded turn reports the new revision.
        if self.native.revision() != self.seen_revision {
            self.retire_read();
            self.channel.discard_pending(CancelReason::Superseded);
            return Ok(Progress {
                read: ReadProgress::Superseded,
                send: Pump::Deferred,
            });
        }
        let send = self.channel.pump(scratch, sink, &mut *clock)?;
        if self.generation != self.channel.observation_generation {
            self.suspend()?;
        }
        Ok(Progress { read, send })
    }
    fn read_step(
        &mut self,
        clock: &mut impl FnMut() -> HostInstant,
        new_id: &mut impl FnMut() -> Result<u128, IdentifierFailure>,
    ) -> Result<ReadProgress<N::Error>, SyncError<N::Error>> {
        if self.reading.is_none() {
            let Some(change) = self.candidate.take() else {
                return Ok(ReadProgress::Idle);
            };
            let now = clock();
            if !self.admit(now)? {
                return Ok(ReadProgress::Suspended);
            }
            let deadline = self.channel.observe(now)?.deadline();
            let id = new_id().map_err(|_| SyncError::Identifier)?;
            if id == 0 {
                return Err(SyncError::Identifier);
            }
            let now = clock();
            if !self.admit(now)? {
                return Ok(ReadProgress::Suspended);
            }
            if now >= deadline {
                return Ok(ReadProgress::Expired);
            }
            self.reading = Some(Reading {
                id,
                revision: change.revision,
                deadline,
                generation: self.channel.observation_generation,
            });
            if let Err(error) = self.native.begin_read() {
                self.retire_read();
                return Ok(ReadProgress::Refused(error));
            }
        }
        let now = clock();
        if !self.admit(now)? {
            return Ok(ReadProgress::Suspended);
        }
        let reading = self.reading.as_ref().expect("active admitted native read");
        if now >= reading.deadline || reading.generation != self.generation {
            self.retire_read();
            return Ok(ReadProgress::Expired);
        }
        let observed = self.native.poll_read();
        let now = clock();
        if !self.admit(now)? {
            return Ok(ReadProgress::Suspended);
        }
        let reading = self.reading.as_ref().expect("native read remains owned");
        if now >= reading.deadline {
            self.retire_read();
            return Ok(ReadProgress::Expired);
        }
        if self.native.revision() != reading.revision {
            self.retire_read();
            return Ok(ReadProgress::Superseded);
        }
        match observed {
            Ok(None) => Ok(ReadProgress::Pending),
            Err(error) => {
                self.retire_read();
                Ok(ReadProgress::Refused(error))
            }
            Ok(Some(text)) => {
                let reading = self.reading.take().expect("complete native read");
                // A genuine watched revision must still be a genuine local read,
                // not a publication that replaced it between native callbacks.
                if text.origin().is_some() {
                    return Ok(ReadProgress::Superseded);
                }
                let offer = self.channel.enqueue_observed(
                    reading.id,
                    text.text(),
                    now,
                    Some(reading.deadline),
                )?;
                match offer {
                    Offer::Queued(stamp) => Ok(ReadProgress::Queued(stamp)),
                    Offer::EchoSuppressed => Ok(ReadProgress::Idle),
                }
            }
        }
    }
    /// Process one already framed inbound record. Native revisions are reported
    /// BEFORE it can publish. Deferred records were not consumed; refusals were.
    /// A returned receipt is retained even when the OS reported `UnknownEffect`.
    pub fn receive(
        &mut self,
        bytes: &[u8],
        clock: impl FnMut() -> HostInstant,
    ) -> Result<Received, SyncError<N::Error>> {
        self.receive_before(bytes, clock, HostInstant::from_micros(u64::MAX))
    }
    /// The immutable ingress deadline includes time waiting for this worker.
    pub fn receive_before(
        &mut self,
        bytes: &[u8],
        mut clock: impl FnMut() -> HostInstant,
        bound: HostInstant,
    ) -> Result<Received, SyncError<N::Error>> {
        let mut turn = Turn {
            owner: self,
            completed: false,
        };
        let settled = match turn.owner.native_changes(&mut clock) {
            Ok(settled) => settled,
            Err(SyncError::Session(SessionError::Clipboard(Error::Disabled)))
                if !turn.owner.is_closed() =>
            {
                turn.owner.suspend()?;
                false
            }
            Err(error) => return Err(error),
        };
        if !settled || !turn.owner.watching {
            turn.completed = true;
            return Ok(Received::Deferred);
        }
        let previous = turn.owner.channel.last_publication;
        let mut publication = PublicationGuard {
            native: &mut turn.owner.native,
            revision: turn.owner.seen_revision,
        };
        let result = turn
            .owner
            .channel
            .receive_before(bytes, &mut publication, &mut clock, bound);
        let result = match result {
            Ok(receipt) => {
                if turn.owner.channel.last_publication != previous {
                    // Native publish cancels the actual read. Do not call foreign
                    // code after its receipt: an extra failure must not erase it.
                    turn.owner.reading = None;
                    turn.owner.candidate = None;
                }
                Received::Consumed(receipt)
            }
            Err(error) if !turn.owner.channel.is_closed() => {
                if turn.owner.generation != turn.owner.channel.observation_generation {
                    turn.owner.suspend()?;
                }
                Received::Refused(error)
            }
            Err(error) => return Err(error.into()),
        };
        turn.completed = true;
        Ok(result)
    }
}
impl<N: NativeClipboard> Drop for Synchronizer<N> {
    fn drop(&mut self) {
        self.close();
    }
}

// Native preparation can service a local copy that arrived AFTER native_changes
// but BEFORE the final OS call. Do not overwrite it with the older incoming item.
struct PublicationGuard<'a, N: NativeClipboard> {
    native: &'a mut N,
    revision: u64,
}
impl<N: NativeClipboard> ClipboardSink for PublicationGuard<'_, N> {
    fn prepare(
        &mut self,
        text: &str,
        stamp: Stamp,
    ) -> Result<(), fr_core::clipboard::PlatformError> {
        self.native.prepare_for_revision(text, stamp, self.revision)
    }
    fn publish(&mut self, text: &str, stamp: Stamp) -> fr_core::clipboard::Publication {
        // Pure check only: no additional blocking work after the core's final
        // clock, deadline and authority check. The native submission itself must
        // retain its OS timestamp/owner fence for changes not yet serviced here.
        if self.native.revision() != self.revision {
            return fr_core::clipboard::Publication::NotSubmitted(
                fr_core::clipboard::PlatformError::LocalChanged,
            );
        }
        self.native.publish(text, stamp)
    }
    /// Same pure revision check, then the deadline travels to the native owner
    /// (an out-of-process owner re-checks it immediately before its OS call).
    fn publish_until(
        &mut self,
        text: &str,
        stamp: Stamp,
        until: HostInstant,
    ) -> fr_core::clipboard::Publication {
        if self.native.revision() != self.revision {
            return fr_core::clipboard::Publication::NotSubmitted(
                fr_core::clipboard::PlatformError::LocalChanged,
            );
        }
        self.native.publish_until(text, stamp, until)
    }
    fn cancel_prepared(&mut self) {
        self.native.cancel_prepared();
    }
}
