//! One bidirectional clipboard owner for an already admitted controller lane.
//!
//! The codecs remain allocation-free; this owner adds the bounded lifecycle:
//! one incoming item, one latest outgoing item, and one release-only cancel.
//! It never reads the OS clipboard, grants authority, or manufactures a viewer
//! lease. Keep it unique for the original input owner and call `maintain` during
//! silence. Transport admission is not remote publication or application paste.
use super::{
    Body, CancelReason, Context, Message, Role, encode,
    receive::{ReceiveError, receive},
    send::Sender,
};
use crate::WireError;
use core::fmt;
use fr_core::clipboard::authority::Monitor;

pub mod egress;
pub mod observation;
use egress::{Egress, Transport};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
pub mod synchronize;
use fr_core::{
    clipboard::{
        ClipboardSession, ClipboardSink, ClipboardSwitch, Error, Publication, Receipt, Stamp,
    },
    input_submission::InputSession,
    limits::ProtocolLimits,
    time::{HostDuration, HostInstant},
};

/// All variants deliberately exclude native/transport strings and clipboard data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionError {
    Wire(WireError),
    Clipboard(Error),
    Transport,
}
impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl core::error::Error for SessionError {}
impl From<WireError> for SessionError {
    fn from(error: WireError) -> Self {
        Self::Wire(error)
    }
}
impl From<Error> for SessionError {
    fn from(error: Error) -> Self {
        Self::Clipboard(error)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Offer {
    Queued(Stamp),
    EchoSuppressed,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Maintenance {
    pub enabled: bool,
    pub incoming_expired: bool,
    pub outgoing_expired: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    Accepted,
    Backpressure,
}
/// Failure may include an unknown external effect. It always closes this lane;
/// neither the current record nor an old item may be replayed after reconnect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransportFailure;

/// A nonblocking, bounded, ordered, all-or-nothing record admission boundary.
/// `Backpressure` MUST mean no bytes were admitted; partial/uncertain admission
/// returns `TransportFailure`. Accepted bytes must be copied/owned before return.
/// Do not use a socket write that can return a partial prefix as this boundary.
/// The transport owns bounded queues and must fence/discard them on lane closure.
/// This call must not wait for capacity, OS work, or an asynchronous completion.
pub trait RecordSink {
    fn try_send(&mut self, record: &[u8]) -> Result<Admission, TransportFailure>;
    /// Asynchronous handoffs retain this permit alongside the exact bytes and
    /// recheck it before transport admission and while transport retains bytes.
    /// The default is for an immediate, already-authorized synchronous sink.
    fn try_send_checked(
        &mut self,
        record: &[u8],
        _permit: Egress,
    ) -> Result<Admission, TransportFailure> {
        self.try_send(record)
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pump {
    Idle,
    Suspended,
    /// The second clock/authority check retired the encoded record. No enqueue.
    Deferred,
    Backpressure,
    RecordAccepted,
    ItemAccepted(Stamp),
    CancelAccepted(Stamp),
}

struct Pending {
    sender: Sender,
    deadline: HostInstant,
    started: bool,
}

/// This is an integration owner, not a substitute for admission/attachment.
/// Numeric scope and channel fields are descriptions, never bearer authority.
pub struct ChannelSession {
    receiver: ClipboardSession,
    transport: Transport,
    payload_live: Option<Arc<AtomicBool>>,
    monitor: Monitor,
    outgoing: Context,
    incoming: Context,
    limits: ProtocolLimits,
    sequence: u64,
    observation_generation: u64,
    last_publication: Option<Stamp>,
    pending: Option<Pending>,
    cancel: Option<(Stamp, CancelReason)>,
}
impl fmt::Debug for ChannelSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClipboardChannelSession")
            .field("closed", &self.is_closed())
            .field("retained_bytes", &self.retained_bytes())
            .field("cancel_pending", &self.cancel.is_some())
            .finish_non_exhaustive()
    }
}

/// Caller scratch is cleared on every return and unwind, including encoding
/// failures. Only the selected record-sized prefix is ever touched or charged.
struct Scratch<'a>(&'a mut [u8]);
impl Drop for Scratch<'_> {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}
/// A caught panic after an external admission/publication attempt must not
/// leave a retryable cursor or sensitive pending item in a reusable owner.
struct ExternalCall<'a> {
    owner: &'a mut ChannelSession,
    completed: bool,
}
impl Drop for ExternalCall<'_> {
    fn drop(&mut self) {
        if !self.completed {
            self.owner.close();
        }
    }
}

impl ChannelSession {
    /// `outgoing` describes the authenticated LOCAL sender on the attached lane.
    /// The opposite sender is derived, not accepted separately from a peer.
    /// `clipboard_granted` is the separately approved, negotiated native grant.
    /// A client integration needs its qualified host-clock authority projection;
    /// it must never construct a synthetic host `InputSession` from wire IDs.
    pub fn new(
        input: &InputSession,
        outgoing: Context,
        limits: ProtocolLimits,
        clipboard_granted: bool,
        now: HostInstant,
    ) -> Result<Self, SessionError> {
        Self::with_monitor(
            Monitor::from_input(input),
            outgoing,
            limits,
            clipboard_granted,
            now,
        )
    }
    /// Use the original owner's read-only authority, including a qualified
    /// controller projection. Neither this context nor this method grants input.
    pub fn with_monitor(
        monitor: Monitor,
        outgoing: Context,
        limits: ProtocolLimits,
        clipboard_granted: bool,
        now: HostInstant,
    ) -> Result<Self, SessionError> {
        let local = outgoing.validate()?;
        let (monitor, live) = egress::lifetime(monitor);
        let receiver =
            ClipboardSession::with_monitor(monitor.clone(), local, limits, clipboard_granted, now)?;
        if receiver.binding() != outgoing.scope {
            return Err(WireError::InvalidBinding.into());
        }
        let incoming = Context {
            sender: match outgoing.sender {
                Role::Host => Role::Controller,
                Role::Controller => Role::Host,
                Role::Observer => return Err(WireError::WrongRole.into()),
            },
            ..outgoing
        };
        let transport = Transport::new(
            monitor.clone(),
            live,
            outgoing,
            limits,
            receiver.local_switch(),
            receiver.peer_switch(),
        );
        Ok(Self {
            transport,
            payload_live: None,
            receiver,
            monitor,
            outgoing,
            incoming,
            limits,
            sequence: 0,
            observation_generation: 0,
            last_publication: None,
            pending: None,
            cancel: None,
        })
    }
    pub fn transport(&self) -> Transport {
        self.transport.clone()
    }
    pub const fn is_closed(&self) -> bool {
        self.receiver.is_closed()
    }
    /// At most two selected-limit item allocations, never an outgoing record list.
    pub fn retained_bytes(&self) -> usize {
        self.receiver.reserved_bytes()
            + self
                .pending
                .as_ref()
                .map_or(0, |p| p.sender.retained_bytes())
    }
    pub fn outgoing_deadline(&self) -> Option<HostInstant> {
        self.pending.as_ref().map(|p| p.deadline)
    }
    pub fn local_switch(&self) -> ClipboardSwitch {
        self.receiver.local_switch()
    }
    pub fn peer_switch(&self) -> ClipboardSwitch {
        self.receiver.peer_switch()
    }
    pub fn set_enabled(&mut self, local: bool, peer: bool) {
        self.receiver.set_enabled(local, peer);
        if !local || !peer {
            self.invalidate_observations();
            self.discard_pending(CancelReason::Disabled);
        }
    }
    /// Does not overwrite an OS clipboard or revoke unrelated input/media.
    /// The transport integration must also fence its already accepted records.
    pub fn close(&mut self) {
        self.transport.close();
        self.invalidate_payload();
        self.receiver.close();
        self.last_publication = None;
        self.pending = None;
        self.cancel = None;
    }
    fn clipboard_error(&mut self, error: Error) -> SessionError {
        if self.receiver.is_closed() {
            self.close();
        } else if error == Error::Disabled {
            self.invalidate_observations();
            self.discard_pending(CancelReason::Disabled);
        }
        error.into()
    }
    // Also fences a read when a different channel operation consumes a switch
    // transition. The owning synchronizer never exposes a mutable channel.
    fn invalidate_observations(&mut self) {
        if let Some(next) = self.observation_generation.checked_add(1) {
            self.observation_generation = next;
        } else {
            self.close();
        }
    }
    fn invalidate_payload(&mut self) {
        if let Some(live) = self.payload_live.take() {
            live.store(false, Ordering::Release);
        }
    }
    fn discard_pending(&mut self, reason: CancelReason) {
        self.invalidate_payload();
        if let Some(pending) = self.pending.take()
            && pending.started
        {
            // No new Begin can pass a pending cancel, so at most one remotely
            // started incomplete item exists. Unsent replacements add no cancel.
            self.cancel = Some((pending.sender.stamp(), reason));
        }
    }
    /// Expiry in either direction clears that direction, independently. An
    /// off/on switch generation is treated as disabled for this maintenance
    /// turn even when both switches are currently on. Old items never resume.
    pub fn maintain(&mut self, now: HostInstant) -> Result<Maintenance, SessionError> {
        let mut state = Maintenance {
            enabled: true,
            incoming_expired: false,
            outgoing_expired: false,
        };
        match self.receiver.maintain(now) {
            Ok(()) => {}
            Err(Error::Disabled) => {
                self.invalidate_observations();
                state.enabled = false;
                self.discard_pending(CancelReason::Disabled);
            }
            Err(Error::Expired) => state.incoming_expired = true,
            Err(error) => {
                self.close();
                return Err(error.into());
            }
        }
        if self.pending.as_ref().is_some_and(|p| now >= p.deadline) {
            state.outgoing_expired = true;
            self.discard_pending(CancelReason::Expired);
        }
        Ok(state)
    }
    /// Report a CURRENT native selection, not an old change notification.
    /// Exact publication provenance suppresses echoes; equal bytes do not.
    /// A genuine local change retires the old send even if the new item cannot
    /// be encoded. `id` must come from qualified randomness, never clipboard data.
    pub fn offer(
        &mut self,
        id: u128,
        text: &str,
        origin: Option<Stamp>,
        now: HostInstant,
    ) -> Result<Offer, SessionError> {
        if !self.maintain(now)?.enabled {
            return Err(Error::Disabled.into());
        }
        match self.receiver.local_change(origin, now) {
            Ok(false) => return Ok(Offer::EchoSuppressed),
            Ok(true) => {}
            Err(error) => return Err(self.clipboard_error(error)),
        }
        self.enqueue_observed(id, text, now, None)
    }
    // The owning synchronizer reports the native revision BEFORE reading it.
    // Enqueuing its completion must not invent a second local revision and
    // invalidate a newer incoming Begin admitted while that read was pending.
    fn enqueue_observed(
        &mut self,
        id: u128,
        text: &str,
        now: HostInstant,
        observation_deadline: Option<HostInstant>,
    ) -> Result<Offer, SessionError> {
        if !self.maintain(now)?.enabled {
            return Err(Error::Disabled.into());
        }
        self.discard_pending(CancelReason::Superseded);
        if id == 0 {
            return Err(WireError::InvalidValue.into());
        }
        let sequence = self.sequence.checked_add(1).ok_or_else(|| {
            self.close();
            SessionError::Clipboard(Error::Limit)
        })?;
        // Consume the source sequence even if bounded allocation fails.
        self.sequence = sequence;
        let stamp = Stamp {
            id,
            source: self.outgoing.validate()?,
            sequence,
        };
        let authority_deadline = self.monitor.deadline(now).map_err(|reason| {
            self.close();
            SessionError::Clipboard(Error::Authority(reason))
        })?;
        let deadline = now
            .checked_add(HostDuration::from_micros(3_000_000))
            .ok_or_else(|| {
                self.close();
                SessionError::Clipboard(Error::Clock)
            })?
            .min(authority_deadline);
        let deadline = observation_deadline.map_or(deadline, |bound| deadline.min(bound));
        if now >= deadline {
            return Err(Error::Expired.into());
        }
        let sender = Sender::new(text, stamp, self.outgoing, self.limits)?;
        self.payload_live = Some(Arc::new(AtomicBool::new(true)));
        self.pending = Some(Pending {
            sender,
            deadline,
            started: false,
        });
        Ok(Offer::Queued(stamp))
    }
    /// At most one bounded record per scheduling turn. Encoding does not grant
    /// permission: the actual clock and original authority are checked AGAIN
    /// immediately before `try_send`. Backpressure never advances the cursor.
    pub fn pump(
        &mut self,
        scratch: &mut [u8],
        sink: &mut impl RecordSink,
        mut clock: impl FnMut() -> HostInstant,
    ) -> Result<Pump, SessionError> {
        let size = scratch
            .len()
            .min(self.limits.max_control_message_bytes() as usize);
        let scratch = Scratch(&mut scratch[..size]);
        let state = self.maintain(clock())?;
        let (stamp, cancellation, len) = if let Some((stamp, reason)) = self.cancel {
            let len = encode(
                Message {
                    stamp,
                    body: Body::Cancel(reason),
                },
                self.outgoing,
                &self.limits,
                scratch.0,
            )?;
            (stamp, true, len)
        } else if !state.enabled {
            return Ok(Pump::Suspended);
        } else if let Some(pending) = &self.pending {
            let Some(len) = pending.sender.encode_next(scratch.0)? else {
                return Ok(Pump::Idle);
            };
            (pending.sender.stamp(), false, len)
        } else {
            return Ok(Pump::Idle);
        };
        let now = clock();
        let state = self.maintain(now)?;
        let still_current = if cancellation {
            self.cancel.is_some_and(|(current, _)| current == stamp)
        } else {
            state.enabled
                && self
                    .pending
                    .as_ref()
                    .is_some_and(|p| p.sender.stamp() == stamp)
        };
        if !still_current {
            return Ok(Pump::Deferred);
        }
        let deadline = if cancellation {
            now.checked_add(HostDuration::from_micros(1_000_000))
                .ok_or(Error::Clock)?
        } else {
            self.pending
                .as_ref()
                .expect("checked current item")
                .deadline
        };
        let permit = Egress::new(
            self.transport.clone(),
            deadline,
            if cancellation {
                None
            } else {
                self.payload_live.clone()
            },
        );
        let mut call = ExternalCall {
            owner: self,
            completed: false,
        };
        let admission = sink
            .try_send_checked(&scratch.0[..len], permit)
            .map_err(|_| SessionError::Transport)?;
        call.completed = true;
        if admission == Admission::Backpressure {
            return Ok(Pump::Backpressure);
        }
        if cancellation {
            call.owner.cancel = None;
            return Ok(Pump::CancelAccepted(stamp));
        }
        let pending = call
            .owner
            .pending
            .as_mut()
            .expect("checked before external call");
        pending.started = true;
        pending.sender.accepted();
        if pending.sender.is_finished() {
            call.owner.pending = None;
            Ok(Pump::ItemAccepted(stamp))
        } else {
            Ok(Pump::RecordAccepted)
        }
    }
    /// Feed the opposite direction through the existing authority/publication
    /// bridge. Malformed framing closes BOTH directions. A remote publication
    /// retires older local work before the native echo notification can arrive.
    pub fn receive(
        &mut self,
        bytes: &[u8],
        sink: &mut impl ClipboardSink,
        clock: impl FnMut() -> HostInstant,
    ) -> Result<Option<Receipt>, SessionError> {
        if self.is_closed() {
            return Err(Error::Closed.into());
        }
        let mut call = ExternalCall {
            owner: self,
            completed: false,
        };
        let result = receive(
            &mut call.owner.receiver,
            bytes,
            call.owner.incoming,
            &call.owner.limits,
            sink,
            clock,
        );
        call.completed = true;
        if call.owner.receiver.is_closed() {
            call.owner.close();
        }
        match result {
            Ok(receipt) => {
                if let Some(receipt) = receipt
                    && !matches!(receipt.publication, Publication::NotSubmitted(_))
                    && call.owner.last_publication != Some(receipt.stamp)
                {
                    call.owner.last_publication = Some(receipt.stamp);
                    call.owner.discard_pending(CancelReason::Superseded);
                }
                Ok(receipt)
            }
            Err(ReceiveError::Clipboard(error)) => Err(call.owner.clipboard_error(error)),
            Err(ReceiveError::Wire(error)) => Err(error.into()),
        }
    }
}
impl Drop for ChannelSession {
    fn drop(&mut self) {
        self.close();
    }
}
