//! Controller clipboard bound to a real accepted grant and its original client.
//!
//! No host InputSession is synthesized from wire IDs. The native/transport
//! integration still supplies the separately approved clipboard capability and
//! dedicated authenticated lane, and fences already accepted bytes on closure.
mod projection;
mod synchronize;
use crate::input::{ClientInstant, InputClient};
use fr_core::{
    clipboard::{ClipboardSink, ClipboardSwitch, Receipt, Stamp},
    limits::ProtocolLimits,
    time::HostInstant,
};
use fr_wire::clipboard::{
    Context, Lane, Role,
    session::{ChannelSession, Offer, Pump, RecordSink, SessionError},
};
use fr_wire::negotiation::ControlBinding;
pub(crate) use projection::Owner;
use std::{
    cell::Cell,
    fmt,
    sync::{Weak, atomic::Ordering},
};
pub use synchronize::{ControllerSynchronizer, NativeError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    NotGranted,
    Permission,
    AlreadyAttached,
    WrongChannel,
    InvalidLimits,
    NotReady,
    Expired,
    Clock,
    Stopped,
    Session(SessionError),
}
impl From<SessionError> for Error {
    fn from(error: SessionError) -> Self {
        Self::Session(error)
    }
}
#[derive(Clone)]
struct ProjectedClock {
    state: Weak<projection::State>,
    fallback: HostInstant,
}
impl ProjectedClock {
    fn sample(&self, now: ClientInstant) -> Result<HostInstant, Error> {
        self.state
            .upgrade()
            .ok_or(Error::Stopped)?
            .sample(now, true)
    }
    /// The core clock callback is infallible. On projection failure, fence FIRST
    /// and return only the previous sample. That sample is unusable authority,
    /// not a fabricated fresh clock; the immediately following check refuses it.
    fn checked(&self, now: ClientInstant, failure: &Cell<Option<Error>>) -> HostInstant {
        match self.sample(now) {
            Ok(at) => at,
            Err(error) => {
                failure.set(Some(error));
                self.state.upgrade().map_or(self.fallback, |state| {
                    state.stopped.store(true, Ordering::Release);
                    state.last_host().unwrap_or(self.fallback)
                })
            }
        }
    }
}
/// Unique channel for this accepted controller. Drop does not keep the input
/// owner alive. Stopping/dropping/replacing that owner permanently fences it.
pub struct ControllerClipboard {
    channel: ChannelSession,
    clock: ProjectedClock,
}
impl fmt::Debug for ControllerClipboard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ControllerClipboard")
            .field("closed", &self.channel.is_closed())
            .finish_non_exhaustive()
    }
}
impl InputClient {
    /// Called only after the authenticated attachment selected the clipboard
    /// capability for this exact session. `granted` includes local OS permission
    /// and approval; request-control negotiation alone is NOT a clipboard grant.
    /// Mapping and genuine presentation must already be ready. The channel may
    /// be attached only once for this input owner, even after a failed/dropped lane.
    pub fn attach_clipboard(
        &mut self,
        channel: u32,
        granted: bool,
        now: ClientInstant,
    ) -> Result<ControllerClipboard, Error> {
        if !granted {
            return Err(Error::Permission);
        }
        let projection = self
            .clipboard_projection
            .as_ref()
            .ok_or(Error::NotGranted)?;
        let parent = projection.parent();
        let outgoing = Context {
            scope: projection.binding(),
            channel,
            sender: Role::Controller,
            lane: Lane::Clipboard,
        };
        self.attach_clipboard_lane(parent, outgoing, self.limits, true, now)
    }
    /// Join the completed route's parent, outgoing context and selected limits
    /// to this ORIGINAL grant. In the native QUIC integration these are exactly
    /// `ClipboardChannel::parent`, `outgoing` and `limits` after `check` succeeds
    /// on the original connection. This checks scope, not transport ownership:
    /// callers must retain that non-cloneable route and fence its accepted bytes.
    /// Limits can only decrease the session ceilings. A narrow lane chunks text
    /// under its real record allowance instead of admitting oversized records.
    pub fn attach_clipboard_lane(
        &mut self,
        parent: ControlBinding,
        outgoing: Context,
        limits: ProtocolLimits,
        granted: bool,
        now: ClientInstant,
    ) -> Result<ControllerClipboard, Error> {
        if !granted {
            return Err(Error::Permission);
        }
        self.tick(now).map_err(|_| Error::Stopped)?;
        let projection = self
            .clipboard_projection
            .as_mut()
            .ok_or(Error::NotGranted)?;
        if parent != projection.parent()
            || outgoing.scope != projection.binding()
            || outgoing.sender != Role::Controller
            || outgoing.lane != Lane::Clipboard
        {
            return Err(Error::WrongChannel);
        }
        let limits = self.limits.negotiated(&limits);
        if limits.max_control_message_bytes() as usize <= fr_wire::clipboard::CHUNK_OVERHEAD {
            return Err(Error::InvalidLimits);
        }
        let (monitor, clock) = projection.attach(outgoing.channel, now)?;
        let at = clock.sample(now)?;
        let channel = ChannelSession::with_monitor(monitor, outgoing, limits, true, at)?;
        Ok(ControllerClipboard { channel, clock })
    }
    pub(crate) fn clipboard_readiness(&self) {
        if let Some(owner) = &self.clipboard_projection {
            let obligations = self
                .pending
                .iter()
                .flatten()
                .map(|p| p.deadline.0)
                .chain(self.control_response_deadline().map(|at| at.0))
                .min()
                .unwrap_or(u64::MAX);
            owner.readiness(self.mapped, self.view_until, obligations);
        }
    }
}
impl ControllerClipboard {
    pub fn local_switch(&self) -> ClipboardSwitch {
        self.channel.local_switch()
    }
    pub fn peer_switch(&self) -> ClipboardSwitch {
        self.channel.peer_switch()
    }
    pub fn retained_bytes(&self) -> usize {
        self.channel.retained_bytes()
    }
    pub fn is_closed(&self) -> bool {
        self.channel.is_closed()
    }
    pub fn close(&mut self) {
        self.channel.close();
    }
    pub fn offer(
        &mut self,
        id: u128,
        text: &str,
        origin: Option<Stamp>,
        now: ClientInstant,
    ) -> Result<Offer, Error> {
        let at = match self.clock.sample(now) {
            Ok(at) => at,
            Err(e) => {
                self.close();
                return Err(e);
            }
        };
        self.channel.offer(id, text, origin, at).map_err(Into::into)
    }
    pub fn pump(
        &mut self,
        scratch: &mut [u8],
        sink: &mut impl RecordSink,
        mut clock: impl FnMut() -> ClientInstant,
    ) -> Result<Pump, Error> {
        let failure = Cell::new(None);
        let mut call = Call::new(&mut self.channel);
        let result = call
            .channel
            .pump(scratch, sink, || self.clock.checked(clock(), &failure));
        call.complete = true;
        result.map_err(|error| failure.get().unwrap_or(Error::Session(error)))
    }
    pub fn receive(
        &mut self,
        bytes: &[u8],
        sink: &mut impl ClipboardSink,
        mut clock: impl FnMut() -> ClientInstant,
    ) -> Result<Option<Receipt>, Error> {
        let failure = Cell::new(None);
        let mut call = Call::new(&mut self.channel);
        let result = call
            .channel
            .receive(bytes, sink, || self.clock.checked(clock(), &failure));
        call.complete = true;
        // Never replace a successful/uncertain external receipt with a later
        // diagnostic. Core checks the projected clock BEFORE native publication.
        result.map_err(|error| failure.get().unwrap_or(Error::Session(error)))
    }
}

// Clock callbacks can unwind before the wire owner's external-operation guard
// is installed. A caught panic must not leave a reusable channel or old payload.
struct Call<'a> {
    channel: &'a mut ChannelSession,
    complete: bool,
}
impl<'a> Call<'a> {
    fn new(channel: &'a mut ChannelSession) -> Self {
        Self {
            channel,
            complete: false,
        }
    }
}
impl Drop for Call<'_> {
    fn drop(&mut self) {
        if !self.complete {
            self.channel.close();
        }
    }
}
