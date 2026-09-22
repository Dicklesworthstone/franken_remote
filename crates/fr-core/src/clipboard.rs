//! Bounded, controller-owned text clipboard transfers (plan 15.3).
//!
//! The original input owner's monitor, not a wire ID or a copied lease, gates
//! every admission and final publication. This module never grants authority,
//! accesses a platform clipboard, or claims that setting it pasted into an app.
//! A runtime must service `maintain` during silence and clear its own queued
//! records when this owner closes. Clipboard permission/approval is separate
//! from permission to inject keys; only the local integration supplies it.
use crate::{
    ids::{InputLeaseId, RemoteSessionId},
    input_submission::{InputSession, Refusal},
    limits::ProtocolLimits,
    time::{HostDuration, HostInstant},
};
use core::fmt;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

pub mod authority;
use authority::Monitor;

pub mod image;

mod receive;

/// Fixed metadata ceiling, independent of the payload byte limit.
pub const MAX_CHUNKS: u32 = 1024;
/// Leaves ample room for the record envelope under the ordinary 64 KiB limit.
pub const MAX_CHUNK_BYTES: usize = 16_384;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Endpoint {
    Host = 1,
    Controller = 2,
}
impl Endpoint {
    #[must_use]
    pub const fn opposite(self) -> Self {
        match self {
            Self::Host => Self::Controller,
            Self::Controller => Self::Host,
        }
    }
}

/// Identifiers describe the admitted channel; possession grants no access.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Binding {
    pub session: RemoteSessionId,
    pub lease: InputLeaseId,
}
impl Binding {
    pub const fn valid(self) -> bool {
        self.session.as_raw() != 0 && self.lease.as_raw() != 0
    }
}
impl fmt::Debug for Binding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ClipboardBinding([redacted])")
    }
}

/// Source sequence is monotonic within the original controller lease. The
/// opaque ID comes from qualified randomness; it is not a bearer credential.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    pub id: u128,
    pub source: Endpoint,
    pub sequence: u64,
}
impl Stamp {
    pub const fn valid(self) -> bool {
        self.id != 0 && self.sequence != 0
    }
}
impl fmt::Debug for Stamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClipboardStamp")
            .field("source", &self.source)
            .field("sequence", &self.sequence)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Begin {
    pub binding: Binding,
    pub stamp: Stamp,
    pub total_bytes: u32,
    pub chunks: u32,
}
impl Begin {
    pub fn validate(self, limits: &ProtocolLimits) -> Result<(), Error> {
        if !self.binding.valid() || !self.stamp.valid() {
            return Err(Error::Binding);
        }
        if self.total_bytes > limits.max_clipboard_item_bytes()
            || self.chunks > MAX_CHUNKS
            || (self.total_bytes == 0) != (self.chunks == 0)
            || self.chunks > self.total_bytes
            || u64::from(self.total_bytes) > u64::from(self.chunks) * MAX_CHUNK_BYTES as u64
        {
            return Err(Error::Limit);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    Permission,
    Disabled,
    Closed,
    Binding,
    Source,
    Replay,
    Busy,
    Limit,
    Allocation,
    UnknownTransfer,
    ChunkOrder,
    Incomplete,
    InvalidUtf8,
    Expired,
    Clock,
    LocalChanged,
    Authority(Refusal),
    Platform(PlatformError),
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl core::error::Error for Error {}

/// Typed native outcomes must never include clipboard text or library strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlatformError {
    Unsupported,
    Permission,
    Unavailable,
    LocalChanged,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum Publication {
    SubmittedToOs,
    NotSubmitted(PlatformError),
    UnknownEffect,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Receipt {
    pub stamp: Stamp,
    pub publication: Publication,
}

/// Preparation may block; publication may not hide a queue, a retry, or a paste
/// keystroke. The owner checks the real clock and original controller again after
/// preparation. `cancel_prepared` is idempotent and must never overwrite a newer
/// OS clipboard. Native implementations must retain `stamp` with their own
/// selection so a local change notification can suppress an echo by provenance.
pub trait ClipboardSink {
    fn prepare(&mut self, text: &str, stamp: Stamp) -> Result<(), PlatformError>;
    fn publish(&mut self, text: &str, stamp: Stamp) -> Publication;
    fn cancel_prepared(&mut self) {}
}

/// Sensitive bytes are neither Clone nor Debug. Clearing is best effort; safe
/// Rust cannot promise that allocators, the OS, or clipboard managers forget.
struct Text(Vec<u8>);
impl Drop for Text {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}
struct Incoming {
    begin: Begin,
    bytes: Text,
    next_chunk: u32,
    deadline: HostInstant,
    local_revision: u64,
}

/// An independent UI/policy switch. Every transition advances its generation,
/// so an off/on cycle during a blocked native preparation still cancels it.
/// Counter exhaustion permanently disables the switch instead of reusing state.
#[derive(Clone)]
pub struct ClipboardSwitch(Arc<AtomicU64>);
impl ClipboardSwitch {
    pub fn set_enabled(&self, enabled: bool) {
        let mut state = self.0.load(Ordering::Acquire);
        loop {
            if state == u64::MAX || (state & 1 == 1) == enabled {
                return;
            }
            match self.0.compare_exchange_weak(
                state,
                state + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(current) => state = current,
            }
        }
    }
    /// Monotonic off/on revision for bounded asynchronous handoffs.
    /// This is metadata, never clipboard or input authority.
    pub fn state(&self) -> u64 {
        self.0.load(Ordering::Acquire)
    }
    pub fn is_enabled(&self) -> bool {
        let state = self.state();
        state != u64::MAX && state & 1 == 1
    }
}

/// One bounded incoming transfer per original native input owner. No mutable
/// monitor escape and no reconstruction of an old lease after reconnect.
pub struct ClipboardSession {
    monitor: Monitor,
    binding: Binding,
    local: Endpoint,
    limits: ProtocolLimits,
    switches: (ClipboardSwitch, ClipboardSwitch),
    switch_states: (u64, u64),
    closed: bool,
    clock: HostInstant,
    incoming: Option<Incoming>,
    received_floor: u64,
    local_revision: u64,
    published: Option<(Receipt, u32)>,
}
impl fmt::Debug for ClipboardSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClipboardSession")
            .field("closed", &self.closed)
            .field("buffered_bytes", &self.buffered_bytes())
            .finish_non_exhaustive()
    }
}
impl ClipboardSession {
    /// `clipboard_granted` is the locally verified negotiated, approved OS
    /// clipboard capability, never a bit accepted from an application record.
    /// Default-on applies only after that grant and a real controller exist.
    pub fn new(
        input: &InputSession,
        local: Endpoint,
        limits: ProtocolLimits,
        clipboard_granted: bool,
        now: HostInstant,
    ) -> Result<Self, Error> {
        Self::with_monitor(
            Monitor::from_input(input),
            local,
            limits,
            clipboard_granted,
            now,
        )
    }
    /// Bind to an existing host owner or a qualified viewer projection. This is
    /// local integration, not authorization from IDs in a clipboard record.
    /// Retain one session per lane/lease; do not reconstruct consumed ledgers.
    pub fn with_monitor(
        monitor: Monitor,
        local: Endpoint,
        limits: ProtocolLimits,
        clipboard_granted: bool,
        now: HostInstant,
    ) -> Result<Self, Error> {
        if !clipboard_granted {
            return Err(Error::Permission);
        }
        let binding = monitor.binding();
        if !binding.valid() {
            return Err(Error::Binding);
        }
        let mut session = Self {
            monitor,
            binding,
            local,
            limits,
            switches: (
                ClipboardSwitch(Arc::new(AtomicU64::new(1))),
                ClipboardSwitch(Arc::new(AtomicU64::new(1))),
            ),
            switch_states: (1, 1),
            closed: false,
            clock: now,
            incoming: None,
            received_floor: 0,
            local_revision: 0,
            published: None,
        };
        session.check(now)?;
        Ok(session)
    }
    pub const fn binding(&self) -> Binding {
        self.binding
    }
    pub fn buffered_bytes(&self) -> usize {
        self.incoming.as_ref().map_or(0, |v| v.bytes.0.len())
    }
    pub fn reserved_bytes(&self) -> usize {
        self.incoming.as_ref().map_or(0, |v| v.bytes.0.capacity())
    }
    pub const fn is_closed(&self) -> bool {
        self.closed
    }
    /// Both endpoints have an independent off switch. Disabling destroys the
    /// incomplete transfer, not the OS clipboard or the consumed sequence floor.
    pub fn set_enabled(&mut self, local: bool, peer: bool) {
        self.switches.0.set_enabled(local);
        self.switches.1.set_enabled(peer);
        if !local || !peer {
            self.incoming = None;
        }
    }
    pub fn local_switch(&self) -> ClipboardSwitch {
        self.switches.0.clone()
    }
    pub fn peer_switch(&self) -> ClipboardSwitch {
        self.switches.1.clone()
    }
    /// Synchronously fences this channel. It never revokes unrelated media or
    /// rewrites OS contents. The runtime must also stop its bounded send queues.
    pub fn close(&mut self) {
        self.closed = true;
        self.incoming = None;
        self.published = None;
    }
    /// A native adapter must report local changes before admitting an incoming
    /// commit. A matching source stamp is our own publication, not a new copy.
    /// Returning false means suppress the echo; it never grants permission to
    /// transmit the item. Genuine local changes supersede an incomplete receive.
    pub fn local_change(&mut self, origin: Option<Stamp>, now: HostInstant) -> Result<bool, Error> {
        self.check(now)?;
        if origin.is_some_and(|stamp| {
            self.published.is_some_and(|(r, _)| {
                r.stamp == stamp && !matches!(r.publication, Publication::NotSubmitted(_))
            })
        }) {
            return Ok(false);
        }
        self.local_revision = self.local_revision.checked_add(1).ok_or_else(|| {
            self.close();
            Error::Limit
        })?;
        Ok(true)
    }
    /// Service this during network silence; a heartbeat never refreshes the
    /// fixed transfer deadline. Authority failures terminally retire this owner.
    pub fn maintain(&mut self, now: HostInstant) -> Result<(), Error> {
        self.check(now)?;
        if self.incoming.as_ref().is_some_and(|v| now >= v.deadline) {
            self.incoming = None;
            return Err(Error::Expired);
        }
        Ok(())
    }
    fn check(&mut self, now: HostInstant) -> Result<HostInstant, Error> {
        if self.closed {
            return Err(Error::Closed);
        }
        if now < self.clock {
            self.monitor.revoke();
            self.close();
            return Err(Error::Clock);
        }
        self.clock = now;
        let deadline = self.monitor.deadline(now).map_err(|reason| {
            self.close();
            Error::Authority(reason)
        })?;
        let states = (self.switches.0.state(), self.switches.1.state());
        let changed = states != self.switch_states;
        self.switch_states = states;
        if changed || !self.switches.0.is_enabled() || !self.switches.1.is_enabled() {
            self.incoming = None;
            return Err(Error::Disabled);
        }
        Ok(deadline)
    }
}
impl Drop for ClipboardSession {
    fn drop(&mut self) {
        self.close();
    }
}
