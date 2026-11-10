//! One explicitly selected microphone on the original running controller.
//!
//! The coordinator owns scheduling, attachment custody and teardown. A native
//! application supplies actual capture through the narrow factory below; no
//! microphone, permission prompt or talk action is invented here.
mod negotiation;

use super::{ControlledViewer, Error as ViewerError, maintenance_gate};
use asupersync::cx::Cx;
use fr_client::input::presentation::PresentedInput;
use fr_core::audio::{MicPermission, MicTalkMode};
use fr_transport::quic::{self, ChannelScope, QuicRecords, Route, audio::AudioUpChannel};
use fr_wire::negotiation::Selection;
use std::{sync::Arc, time::Duration};

/// Content-free native capture failures, without an FFI dependency in frd.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureFailure {
    Configuration,
    Selection,
    Unavailable,
    Denied,
    Expired,
    Clock,
    Backpressure,
    Protocol,
    Spawn,
    Native,
    Stopped,
    Discontinuity,
    Allocation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    AlreadyAttached,
    NotSelected,
    WrongBinding,
    Clock,
    SetupExpired,
    Cancelled,
    Closed,
    Busy,
    InvalidRecord,
    /// The original optional microphone attachment's receive framing expired.
    ReceiveExpired,
    EgressExpired,
    Order,
    CaptureTransport,
    Capture(CaptureFailure),
    Consent(fr_client::audio::uplink::Error),
    Transport(quic::Error),
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "viewer-microphone: {self:?}")
    }
}
impl std::error::Error for Error {}

/// A factory refusal returns custody of the exact non-cloneable lane.
pub struct AttachError {
    reason: Error,
    lane: Box<AudioUpChannel>,
}
impl std::fmt::Debug for AttachError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ViewerMicrophoneAttachError")
            .field("reason", &self.reason)
            .finish_non_exhaustive()
    }
}
impl AttachError {
    pub fn new(reason: Error, lane: AudioUpChannel) -> Self {
        Self {
            reason,
            lane: Box::new(lane),
        }
    }
    pub const fn reason(&self) -> Error {
        self.reason
    }
    pub fn into_parts(self) -> (Error, AudioUpChannel) {
        (self.reason, *self.lane)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Negotiating,
    Muted,
    AwaitingHost,
    Starting,
    Talking,
    Stopped,
}
/// Actual process retirement evidence, separate from admission stopping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retirement {
    NotStarted,
    Pending,
    Complete,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Status {
    pub state: State,
    pub permission: MicPermission,
    pub reason: Option<Error>,
    pub retirement: Retirement,
}

/// The actual native capture/network owner. Every method is nonblocking.
/// The operation check retains the SAME immutable queued-output guards on
/// the original transport, including media, clock and recovery operations.
/// It is true when no audio remains in custody; it cannot renew old output.
pub trait NativeMicrophone: Send {
    fn operation_check(&self) -> Arc<dyn Fn() -> bool + Send + Sync>;
    fn owns_inbound(&self, route: Route) -> bool;
    fn permits_io(&self) -> bool;
    /// Collect original native drain witnesses before checking old guards.
    fn maintain(&mut self, q: &mut QuicRecords) -> Result<(), Error>;
    fn service(
        &mut self,
        q: &mut QuicRecords,
        authorize: &mut dyn FnMut() -> bool,
    ) -> Result<(), Error>;
    fn set_permission(&mut self, permission: MicPermission) -> Result<(), Error>;
    fn begin_talk(&mut self, mode: MicTalkMode) -> Result<(), Error>;
    fn mute(&mut self) -> Result<(), Error>;
    fn stop(&mut self);
    fn retire(&mut self, q: &mut QuicRecords) -> Result<(), Error>;
    fn status(&self) -> Status;
}

type Factory = Box<
    dyn FnOnce(
            &mut QuicRecords,
            AudioUpChannel,
            &mut PresentedInput,
            &Selection,
            &Cx,
        ) -> Result<Box<dyn NativeMicrophone>, AttachError>
        + Send,
>;

#[derive(Default)]
pub(super) struct Slot {
    used: bool,
    pending: Option<negotiation::Pending>,
    native: Option<Box<dyn NativeMicrophone>>,
    pub(super) failure: Option<Error>,
    stopped: bool,
}
impl Slot {
    pub(super) fn owns(&self, route: Route, bytes: &[u8]) -> bool {
        self.pending
            .as_ref()
            .is_some_and(|pending| pending.owns(route, bytes))
            || self
                .native
                .as_ref()
                .is_some_and(|native| native.owns_inbound(route))
    }
    pub(super) fn deadline_us(&self) -> Option<u64> {
        self.pending.as_ref().map(negotiation::Pending::deadline)
    }
    pub(super) fn check_pending(&mut self) -> Result<(), Error> {
        if let Some(pending) = &mut self.pending {
            pending.check()?;
        }
        Ok(())
    }
    pub(super) fn permits_io(&mut self) -> bool {
        self.check_pending().is_ok()
            && self
                .native
                .as_ref()
                .is_none_or(|native| native.permits_io())
    }
    pub(super) fn maintain(&mut self, q: &mut QuicRecords) -> Result<(), Error> {
        self.check_pending()?;
        if let Some(native) = &mut self.native {
            native.maintain(q)?;
        }
        Ok(())
    }
    pub(super) fn stop(&mut self, q: &mut QuicRecords) {
        if !self.used {
            return;
        }
        self.stopped = true;
        if self.pending.take().is_some() {
            self.failure.get_or_insert(Error::Cancelled);
            // The peer may still send the final exchange record. There is no
            // established optional pair whose retirement could isolate it.
            q.close();
        }
        if let Some(native) = &mut self.native {
            native.stop();
            if let Err(error) = native.retire(q) {
                self.failure.get_or_insert(error);
                q.close();
            }
        }
    }
    fn status(&self) -> Option<Status> {
        if let Some(native) = &self.native {
            let mut status = native.status();
            status.reason = status.reason.or(self.failure);
            return Some(status);
        }
        self.used.then_some(Status {
            state: if self.pending.is_some() {
                State::Negotiating
            } else {
                State::Stopped
            },
            permission: MicPermission::NotRequested,
            reason: self.failure,
            retirement: Retirement::NotStarted,
        })
    }
}

impl ControlledViewer {
    /// Expect one role-8 offer from this original authenticated host. The
    /// fixed timeout starts NOW and no waiting turn extends it. The factory
    /// receives the original presented controller only after the selected
    /// media tuple and the completed attachment have both been checked.
    ///
    /// Construct a muted owner: OS permission and visible local talk remain
    /// independent later actions. Negotiation opens no native source.
    pub fn expect_microphone<F>(&mut self, timeout: Duration, factory: F) -> Result<(), Error>
    where
        F: FnOnce(
                &mut QuicRecords,
                AudioUpChannel,
                &mut PresentedInput,
                &Selection,
                &Cx,
            ) -> Result<Box<dyn NativeMicrophone>, AttachError>
            + Send
            + 'static,
    {
        if self.microphone.used {
            return Err(Error::AlreadyAttached);
        }
        self.check().map_err(|_| Error::Closed)?;
        if !fr_wire::audio_up::uplink_selected(&self.session.opened.selection) {
            return Err(Error::NotSelected);
        }
        let pending = negotiation::Pending::new(
            &self.session.cx,
            &mut self.session.transport,
            ChannelScope {
                control: self.session.routes,
                parent: self.session.opened.binding,
                selection: &self.session.opened.selection,
            },
            self.media.as_ref().ok_or(Error::Closed)?.binding(),
            timeout,
            Box::new(factory),
        )?;
        self.microphone.used = true;
        self.microphone.pending = Some(pending);
        Ok(())
    }
    pub fn microphone_status(&self) -> Option<Status> {
        self.microphone.status()
    }
    /// A positive OS permission result never starts recording automatically.
    pub fn set_microphone_permission(&mut self, permission: MicPermission) -> Result<(), Error> {
        if self.is_closed() || self.microphone.stopped {
            return Err(Error::Closed);
        }
        self.microphone
            .native
            .as_mut()
            .ok_or(Error::Busy)?
            .set_permission(permission)
    }
    /// An explicit visible local talk action on this same controller.
    pub fn begin_microphone_talk(&mut self, mode: MicTalkMode) -> Result<(), Error> {
        self.check().map_err(|_| Error::Closed)?;
        if self.microphone.stopped {
            return Err(Error::Closed);
        }
        self.microphone
            .native
            .as_mut()
            .ok_or(Error::Busy)?
            .begin_talk(mode)
    }
    /// Fence capture immediately, even when input or the peer has ended.
    pub fn mute_microphone(&mut self) -> Result<(), Error> {
        self.microphone
            .native
            .as_mut()
            .map_or(Ok(()), |native| native.mute())
    }
    /// Terminal for this attachment; actual process evidence stays readable.
    /// A new expectation cannot recycle the original microphone scope.
    pub fn retire_microphone(&mut self) -> Result<(), Error> {
        self.microphone.stop(&mut self.session.transport);
        if self.session.transport.is_closed() {
            self.close();
        }
        self.microphone.failure.map_or(Ok(()), Err)
    }
    pub(super) fn service_microphone(&mut self) -> Result<(), ViewerError> {
        let result = self.service_microphone_inner();
        if let Err(error) = result {
            self.microphone.failure.get_or_insert(error);
        }
        result.map_err(ViewerError::Microphone)
    }
    fn service_microphone_inner(&mut self) -> Result<(), Error> {
        self.microphone.maintain(&mut self.session.transport)?;
        if self.microphone.stopped {
            return Ok(());
        }
        if let Some(pending) = &mut self.microphone.pending {
            let ready = pending.advance(&mut self.session.transport, || {
                maintenance_gate(
                    &mut self.input,
                    &self.control,
                    &self.session.cx,
                    &std::cell::Cell::new(None),
                )
            })?;
            if ready {
                let pending = self.microphone.pending.take().ok_or(Error::Closed)?;
                let native = pending.start(&mut self.session.transport, &mut self.input)?;
                self.microphone.native = Some(native);
            }
        }
        if let Some(native) = &mut self.microphone.native {
            native.service(&mut self.session.transport, &mut || {
                maintenance_gate(
                    &mut self.input,
                    &self.control,
                    &self.session.cx,
                    &std::cell::Cell::new(None),
                )
            })?;
        }
        Ok(())
    }
}
