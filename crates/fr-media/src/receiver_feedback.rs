//! Single-credit solicited receiver metrics. Absolute sample validity starts at
//! the host query, not reply receipt; neither clock synchronization nor a fresh
//! nonce is needed for advisory metrics on an authenticated one-owner connection.
use crate::pacing::ReceiverEvidence;
use fr_core::limits::ProtocolLimits;
use fr_wire::{
    WireError,
    decoder::Binding,
    input::{
        InputDelivery::Reliable,
        InputDirection::{HostToViewer, ViewerToHost},
    },
    receiver_metrics::{self as wire, Load, Message},
};

pub const QUERY_INTERVAL_US: u64 = 50_000;
pub const SAMPLE_LIFETIME_US: u64 = 150_000;
pub const REPLY_LIFETIME_US: u64 = 100_000;
const MIN_SAMPLE_INTERVAL_US: u64 = 25_000;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Wire(WireError),
    Clock,
    Sequence,
    Expired,
}
/// Result of attempting the exact pending telemetry record. Expired means
/// its ORIGINAL deadline elapsed before transport admission, not that retained
/// bytes or the connection failed. No admission may be reported twice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    Accepted,
    Backpressure,
    Expired,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendError<E> {
    Protocol(Error),
    Transport(E),
}
struct Query {
    bytes: [u8; wire::QUERY_BYTES],
    sequence: u64,
    until: u64,
    sent: bool,
}
/// Bound to one original media/control tuple by its enclosing session. This
/// state cannot be cloned or reset when a query is backpressured or times out.
pub struct Requester {
    binding: Binding,
    limits: ProtocolLimits,
    sequence: u64,
    next: u64,
    last_now: u64,
    pending: Option<Query>,
    sample: Option<(Load, u64)>,
}
impl Requester {
    pub fn new(binding: Binding, limits: ProtocolLimits, now: u64) -> Result<Self, Error> {
        binding.validate().map_err(Error::Wire)?;
        Ok(Self {
            binding,
            limits,
            sequence: 0,
            next: now,
            last_now: now,
            pending: None,
            sample: None,
        })
    }
    fn tick(&mut self, now: u64) -> Result<(), Error> {
        if now < self.last_now {
            return Err(Error::Clock);
        }
        self.last_now = now;
        if self.pending.as_ref().is_some_and(|p| now >= p.until) {
            self.pending = None;
        }
        if self.sample.is_some_and(|(_, until)| now >= until) {
            self.sample = None;
        }
        Ok(())
    }
    pub fn prepare(&mut self, now: u64) -> Result<(), Error> {
        self.tick(now)?;
        if self.pending.is_none() && now >= self.next {
            let sequence = self.sequence.checked_add(1).ok_or(Error::Sequence)?;
            let until = now.checked_add(SAMPLE_LIFETIME_US).ok_or(Error::Clock)?;
            let next = now.checked_add(QUERY_INTERVAL_US).ok_or(Error::Clock)?;
            let mut bytes = [0; wire::QUERY_BYTES];
            wire::encode(
                Message::Query { sequence },
                self.binding,
                &self.limits,
                &mut bytes,
                HostToViewer,
                Reliable,
            )
            .map_err(Error::Wire)?;
            self.sequence = sequence;
            self.next = next;
            self.pending = Some(Query {
                bytes,
                sequence,
                until,
                sent: false,
            });
        }
        Ok(())
    }
    pub fn pending(&mut self, now: u64) -> Result<Option<(&[u8], u64)>, Error> {
        self.tick(now)?;
        Ok(self
            .pending
            .as_ref()
            .filter(|q| !q.sent)
            .map(|q| (q.bytes.as_slice(), q.until)))
    }
    pub fn queued(&mut self, now: u64) -> Result<(), Error> {
        self.tick(now)?;
        let q = self.pending.as_mut().ok_or(Error::Expired)?;
        if q.sent {
            return Err(Error::Sequence);
        }
        q.sent = true;
        Ok(())
    }
    /// Submit at most one query. Backpressure retains its exact bytes and
    /// deadline; expiry retires it without fabricating a sent query, resampling
    /// evidence, or issuing a replacement in this call. The transport callback
    /// must report Expired only before admission and at/after `until`.
    pub fn transmit<E>(
        &mut self,
        now: u64,
        send: impl FnOnce(&[u8], u64) -> Result<Admission, E>,
    ) -> Result<bool, SendError<E>> {
        self.prepare(now).map_err(SendError::Protocol)?;
        let Some((bytes, until)) = self.pending(now).map_err(SendError::Protocol)? else {
            return Ok(false);
        };
        let admitted = send(bytes, until).map_err(SendError::Transport)?;
        match admitted {
            Admission::Accepted => self.queued(now).map_err(SendError::Protocol)?,
            Admission::Backpressure => {}
            Admission::Expired => {
                // The callback established this lower bound on the same clock.
                // Advance only the existing owner; do not prepare a new query.
                self.tick(until).map_err(SendError::Protocol)?;
            }
        }
        Ok(admitted == Admission::Accepted)
    }
    /// Late/replayed replies are consumed without refreshing any evidence. A
    /// response to a never-issued query is a protocol error. Returns true only
    /// for the one matching, sent, still-live query.
    pub fn receive(&mut self, bytes: &[u8], now: u64) -> Result<bool, Error> {
        self.tick(now)?;
        let Message::Reply { sequence, load } =
            wire::decode(bytes, self.binding, &self.limits, ViewerToHost, Reliable)
                .map_err(Error::Wire)?
        else {
            return Err(Error::Sequence);
        };
        if sequence > self.sequence {
            return Err(Error::Sequence);
        }
        let Some(q) = &self.pending else {
            return Ok(false);
        };
        if sequence != q.sequence {
            return Ok(false);
        }
        if !q.sent {
            return Err(Error::Sequence);
        }
        self.sample = Some((load, q.until));
        self.pending = None;
        Ok(true)
    }
    /// Copy the current solicited sample with its ORIGINAL host-query expiry.
    /// A caller transferring it to another bounded owner must preserve that
    /// deadline: neither a read nor reply receipt buys a new sample lifetime.
    /// Missing and expired evidence stays unknown; this never prepares a query.
    pub fn sample(&mut self, now: u64) -> Result<Option<(Load, u64)>, Error> {
        self.tick(now)?;
        Ok(self.sample)
    }
    pub fn evidence(&mut self, now: u64) -> Result<ReceiverEvidence, Error> {
        Ok(self
            .sample(now)?
            .map_or(ReceiverEvidence::Unknown, |(load, _)| {
                ReceiverEvidence::Measured(load)
            }))
    }
}
struct Reply {
    bytes: [u8; wire::REPLY_BYTES],
    until: u64,
}
/// One immutable unsent reply. Expired work is retired rather than resampled;
/// bursts or a slow reverse stream cannot create an unbounded telemetry queue.
pub struct Responder {
    binding: Binding,
    limits: ProtocolLimits,
    sequence: u64,
    next_sample: u64,
    last_now: u64,
    pending: Option<Reply>,
}
impl Responder {
    pub fn new(binding: Binding, limits: ProtocolLimits, now: u64) -> Result<Self, Error> {
        binding.validate().map_err(Error::Wire)?;
        Ok(Self {
            binding,
            limits,
            sequence: 0,
            next_sample: now,
            last_now: now,
            pending: None,
        })
    }
    fn tick(&mut self, now: u64) -> Result<(), Error> {
        if now < self.last_now {
            return Err(Error::Clock);
        }
        self.last_now = now;
        if self.pending.as_ref().is_some_and(|p| now >= p.until) {
            self.pending = None;
        }
        Ok(())
    }
    pub fn receive(&mut self, bytes: &[u8], load: Load, now: u64) -> Result<bool, Error> {
        self.tick(now)?;
        let Message::Query { sequence } =
            wire::decode(bytes, self.binding, &self.limits, HostToViewer, Reliable)
                .map_err(Error::Wire)?
        else {
            return Err(Error::Sequence);
        };
        if sequence <= self.sequence {
            return Ok(false);
        }
        self.sequence = sequence;
        // Consume a burst without displacing the existing response or pinning
        // unrelated renewal behind telemetry on the shared control stream.
        if self.pending.is_some() || now < self.next_sample {
            return Ok(false);
        }
        let until = now.checked_add(REPLY_LIFETIME_US).ok_or(Error::Clock)?;
        let next = now
            .checked_add(MIN_SAMPLE_INTERVAL_US)
            .ok_or(Error::Clock)?;
        let mut bytes = [0; wire::REPLY_BYTES];
        wire::encode(
            Message::Reply { sequence, load },
            self.binding,
            &self.limits,
            &mut bytes,
            ViewerToHost,
            Reliable,
        )
        .map_err(Error::Wire)?;
        self.pending = Some(Reply { bytes, until });
        self.next_sample = next;
        Ok(true)
    }
    pub fn pending(&mut self, now: u64) -> Result<Option<(&[u8], u64)>, Error> {
        self.tick(now)?;
        Ok(self.pending.as_ref().map(|p| (p.bytes.as_slice(), p.until)))
    }
    /// Submit at most the original reply. An expired, unadmitted reply is
    /// retired, never resampled for its consumed query. A successful callback
    /// means transport admission only, not receipt or presentation evidence.
    pub fn transmit<E>(
        &mut self,
        now: u64,
        send: impl FnOnce(&[u8], u64) -> Result<Admission, E>,
    ) -> Result<bool, SendError<E>> {
        let Some((bytes, until)) = self.pending(now).map_err(SendError::Protocol)? else {
            return Ok(false);
        };
        let admitted = send(bytes, until).map_err(SendError::Transport)?;
        match admitted {
            Admission::Accepted => self.queued(now).map_err(SendError::Protocol)?,
            Admission::Backpressure => {}
            Admission::Expired => self.tick(until).map_err(SendError::Protocol)?,
        }
        Ok(admitted == Admission::Accepted)
    }
    pub fn queued(&mut self, now: u64) -> Result<(), Error> {
        self.tick(now)?;
        self.pending.take().ok_or(Error::Expired)?;
        Ok(())
    }
}
