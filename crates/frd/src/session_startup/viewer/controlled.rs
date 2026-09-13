//! The running native viewer owns input identities, freshness and network sends.
//! Decode/present work stays off this task; qualified callbacks arrive between turns.
pub mod events;
mod viewport;
use super::{ViewerSession, now};
use crate::{
    input_quic::{self, NegotiatedInput},
    media::clock::{self, ClockSync},
    media_quic::NegotiatedMedia,
};
use asupersync::{cx::Cx, types::CancelKind};
use fr_client::input::{
    self, Action, ClientInstant, Encoded, ResultEvent, StopReason,
    held::EncodedHeldState,
    presentation::{self, PresentedInput},
};
use fr_core::{held_state::HeldState, input::DesktopPoint};
use fr_media::{delivery::DecodedFrame, freshness::ViewEvidence};
use fr_transport::quic::{self, Disposition, Route};
use fr_wire::{Kind, input::MAX_INPUT_RECORD_BYTES, negotiation::Role};
use std::{
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Session(super::Error),
    Input(input_quic::Error),
    View(presentation::Error),
    Viewport(fr_client::input::viewport::Error),
    Clock(clock::Error),
    Media(crate::media_quic::Error),
    WrongBinding,
    ClockNotReady,
    Backpressure,
    Expired,
    Closed,
    Capture(events::Error),
}
impl From<super::Error> for Error {
    fn from(e: super::Error) -> Self {
        Self::Session(e)
    }
}
impl From<presentation::Error> for Error {
    fn from(e: presentation::Error) -> Self {
        Self::View(e)
    }
}
/// An independently usable lifecycle fence. It cannot resume or acquire control.
/// The platform calls stop on focus loss, hiding, suspend or local disconnect.
#[derive(Clone)]
pub struct ViewerControl {
    cx: Cx,
    stopped: Arc<AtomicBool>,
}
impl ViewerControl {
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        self.cx.cancel_fast(CancelKind::User);
    }
    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }
}
struct Pending {
    bytes: [u8; MAX_INPUT_RECORD_BYTES],
    len: usize,
    until: u64,
}
/// Consumes one existing explicit grant and its exact media/input/clock owners.
/// One fixed-size pending record holds an action, pointer OR held-state snapshot.
/// Subsequent UI events are backpressured BEFORE consuming another identity.
pub struct ControlledViewer {
    session: ViewerSession,
    input: PresentedInput,
    viewport: fr_client::input::viewport::Viewport,
    channels: NegotiatedInput,
    media: NegotiatedMedia,
    clock: ClockSync,
    clock_at: u64,
    pending: Option<Pending>,
    control: ViewerControl,
    last_result: Option<ResultEvent>,
    events: Option<events::Receiver>,
}
impl std::fmt::Debug for ControlledViewer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlledViewer")
            .field("closed", &self.is_closed())
            .field("pending_send", &self.pending.is_some())
            .finish_non_exhaustive()
    }
}
impl ViewerSession {
    /// Transfer the SAME session-owned exchange, including pending probe bytes
    /// and its original correlation, rather than constructing another estimator.
    /// Presentation, negotiated input and the initial control ticket remain
    /// independent prerequisites. A missing/unmeasured clock refuses control.
    pub fn into_controlled_synchronized(
        mut self,
        channels: NegotiatedInput,
        media: NegotiatedMedia,
        input: PresentedInput,
    ) -> Result<ControlledViewer, Error> {
        let clock = self.clock.take().ok_or(Error::ClockNotReady)?;
        self.into_controlled(channels, media, input, clock)
    }

    /// `ClockSync` must already have measured THIS session. `PresentedInput` must
    /// come from the actual decoder receiver, with a mapped, qualified visible
    /// frame and network-bounded initial ticket. No prerequisite is fabricated.
    pub fn into_controlled(
        mut self,
        channels: NegotiatedInput,
        media: NegotiatedMedia,
        mut input: PresentedInput,
        mut clock: ClockSync,
    ) -> Result<ControlledViewer, Error> {
        self.check()?;
        let binding = self.opened.binding;
        let limits = self.opened.selection.limits;
        let (parent, configuration) = channels
            .viewer_scope(&self.transport)
            .map_err(Error::Input)?;
        let b = media.binding();
        let v = input.input_view();
        if self.clock.is_some()
            || self.opened.selection.role != Role::RequestControl
            || parent != binding
            || configuration != b
            || input.binding().channel != channels.channel_binding()
            || input.binding().session != binding.remote_session
            || input.host_boot() != binding.host_boot
            || input.protocol_limits() != limits
            || channels.limits() != limits
            || *media.limits().protocol() != limits
            || input.media_bindings() != media.bindings()
            || v.geometry != b.geometry
            || v.viewport != b.viewport
            || v.configuration != b.configuration
            || v.recovery != b.recovery
            || input.ticket_deadline().is_none()
            || !clock.matches_viewer(&self.transport, binding, limits)
        {
            return Err(Error::WrongBinding);
        }
        media.check(&self.transport).map_err(Error::Media)?;
        let sample = clock
            .correlation(&mut self.transport)
            .map_err(Error::Clock)?
            .ok_or(Error::ClockNotReady)?;
        let t = ClientInstant(now(&self.cx)?);
        if input.clock_correlation() != sample {
            input.synchronize(sample, t)?;
        }
        input.view_deadline(t)?;
        input.enable_control_renewal(binding.id, t)?;
        let control = ViewerControl {
            cx: self.cx.clone(),
            stopped: Arc::new(AtomicBool::new(false)),
        };
        let viewport = input.viewport();
        Ok(ControlledViewer {
            session: self,
            input,
            viewport,
            channels,
            media,
            clock,
            clock_at: sample.received_at_us(),
            pending: None,
            control,
            last_result: None,
            events: None,
        })
    }
}
impl ControlledViewer {
    pub(super) fn streaming_parts(
        &mut self,
    ) -> Result<(&mut ViewerSession, &NegotiatedMedia), Error> {
        self.check()?;
        Ok((&mut self.session, &self.media))
    }
    pub(super) fn check_stream_receiver(
        &self,
        receiver: &fr_media::delivery::ReceivePipeline,
    ) -> Result<(), Error> {
        self.input.check_receiver(receiver).map_err(Error::View)
    }
    pub fn control(&self) -> ViewerControl {
        self.control.clone()
    }
    pub fn is_closed(&self) -> bool {
        self.control.is_stopped() || self.session.is_closed()
    }
    pub fn pending_send(&self) -> bool {
        self.pending.is_some()
    }
    pub fn pending_actions(&self) -> usize {
        self.input.pending_actions()
    }
    pub fn last_result(&self) -> Option<ResultEvent> {
        self.last_result
    }
    pub fn close(&mut self) {
        self.viewport.stop();
        self.input.stop(StopReason::Disconnected);
        self.pending = None;
        self.control.stop();
        self.events = None;
        self.clock.stop();
        self.session.close();
    }
    fn check_inner(&mut self) -> Result<ClientInstant, Error> {
        if self.is_closed() {
            return Err(Error::Closed);
        }
        self.session.check()?;
        self.channels
            .viewer_routes(&self.session.transport)
            .map_err(Error::Input)?;
        self.media
            .check(&self.session.transport)
            .map_err(Error::Media)?;
        let t = ClientInstant(now(&self.session.cx)?);
        self.input.maintenance_deadline(t)?;
        if let Some(events) = &self.events {
            events.check(t).map_err(Error::Capture)?;
        }
        if self.pending.as_ref().is_some_and(|p| t.0 >= p.until) {
            return Err(Error::Expired);
        }
        Ok(t)
    }
    fn check(&mut self) -> Result<ClientInstant, Error> {
        let result = self.check_inner();
        if result.is_err() {
            self.close();
        }
        result
    }
    fn slot(&mut self) -> Result<ClientInstant, Error> {
        let t = self.check()?;
        if self.pending.is_some() {
            return Err(Error::Backpressure);
        }
        self.input.view_deadline(t)?;
        Ok(t)
    }
    /// Queue the exact original bytes; failure before encoding consumes no ID.
    /// Backpressure after encoding is internal, never an instruction to retry
    /// the same action by generating fresh credentials or sequence positions.
    pub fn action(&mut self, action: Action<'_>) -> Result<Encoded, Error> {
        let t = self.slot()?;
        let until = self.input.send_deadline(t)?.0;
        let mut p = Pending {
            bytes: [0; MAX_INPUT_RECORD_BYTES],
            len: 0,
            until,
        };
        let result = self.input.action(action, &mut p.bytes, t)?;
        p.len = result.bytes;
        self.pending = Some(p);
        Ok(result)
    }
    pub fn pointer(&mut self, position: DesktopPoint) -> Result<Encoded, Error> {
        let t = self.slot()?;
        let until = self.input.send_deadline(t)?.0;
        let mut p = Pending {
            bytes: [0; MAX_INPUT_RECORD_BYTES],
            len: 0,
            until,
        };
        let result = self.input.pointer(position, &mut p.bytes, t)?;
        p.len = result.bytes;
        self.pending = Some(p);
        Ok(result)
    }
    /// Actual platform snapshots only. One pending slot preserves their ordering
    /// relative to key/button transitions; an expired ticket does not bar releases.
    pub fn reconcile_held(
        &mut self,
        observed: HeldState,
    ) -> Result<Option<EncodedHeldState>, Error> {
        let t = self.slot()?;
        let until = self.input.view_deadline(t)?.0;
        let mut p = Pending {
            bytes: [0; MAX_INPUT_RECORD_BYTES],
            len: 0,
            until,
        };
        let Some(result) = self.input.reconcile_held(observed, &mut p.bytes, t)? else {
            return Ok(None);
        };
        p.len = result.bytes;
        self.pending = Some(p);
        Ok(Some(result))
    }
    /// A completion token from the actual receiver/decoder is not visibility.
    pub fn decoded(&mut self, frame: DecodedFrame, submitted: bool) -> Result<(), Error> {
        let t = self.check()?;
        let result = self.input.decoded(frame, submitted, t).map_err(Error::View);
        if self.input.stopped().is_some() {
            self.close();
        }
        result
    }
    /// Only the qualified platform presentation path may confirm this frame.
    pub fn visible(&mut self, frame: u64) -> Result<ViewEvidence, Error> {
        let t = self.check()?;
        let result = self.input.visible(frame, t).map_err(Error::View);
        if self.input.stopped().is_some() {
            self.close();
        }
        result
    }
    /// Results already retained from this original authenticated feedback stream
    /// remain interpretable after closure. No network, retry or rollback occurs.
    pub fn retained_result(
        &mut self,
        bytes: &[u8],
        at: ClientInstant,
    ) -> Result<ResultEvent, Error> {
        if !self.is_closed() {
            return Err(Error::WrongBinding);
        }
        let result = self.input.result(bytes, at)?;
        self.last_result = Some(result);
        Ok(result)
    }
    fn send(&mut self) -> Result<(), Error> {
        let t = self.check_inner()?;
        let cx = &self.session.cx;
        let q = &mut self.session.transport;
        if let Some(until) = self.input.control_response_deadline() {
            // Copy only one bounded response: the final transport callback must
            // be able to recheck the mutable view owner immediately before send.
            let mut bytes = [0; fr_wire::authority::OBSERVATION_RESPONSE_BYTES + 16];
            if let Some(response) = self.input.pending_control_response(t)? {
                bytes.copy_from_slice(response);
                match q.send(
                    cx,
                    Route::Stream(self.session.routes.outbound),
                    &bytes,
                    until.0,
                    || permitted(&mut self.input, &self.control, cx),
                ) {
                    Ok(()) => self.input.control_response_sent(ClientInstant(now(cx)?))?,
                    Err(quic::Error::Backpressure) => {}
                    Err(e) => return Err(Error::Session(e.into())),
                }
            }
        }
        if let Some(p) = &self.pending {
            match self.channels.send(cx, q, &p.bytes[..p.len], p.until, || {
                permitted(&mut self.input, &self.control, cx)
            }) {
                Ok(()) => self.pending = None,
                Err(input_quic::Error::Transport(quic::Error::Backpressure)) => {}
                Err(e) => return Err(Error::Input(e)),
            }
        }
        Ok(())
    }
    fn step(
        &mut self,
        result: &mut impl FnMut(ResultEvent),
        other: &mut impl FnMut(Route, &[u8]) -> Result<Disposition, ()>,
    ) -> Result<(), Error> {
        self.check_inner()?;
        self.clock
            .receive(&mut self.session.transport, |_, _| Ok(Disposition::Blocked))
            .map_err(Error::Clock)?;
        self.clock
            .service(&mut self.session.transport)
            .map_err(Error::Clock)?;
        let sample = self
            .clock
            .correlation(&mut self.session.transport)
            .map_err(Error::Clock)?
            .ok_or(Error::ClockNotReady)?;
        if sample.received_at_us() != self.clock_at {
            self.input
                .synchronize(sample, ClientInstant(now(&self.session.cx)?))?;
            self.clock_at = sample.received_at_us();
        }
        self.dispatch_captured()?;
        self.send()?;
        let (_, feedback, _) = self
            .channels
            .viewer_routes(&self.session.transport)
            .map_err(Error::Input)?;
        let progress = self
            .media
            .progress_route(&self.session.transport)
            .map_err(Error::Media)?;
        let limits = self.media.limits();
        let inbound = self.session.routes.inbound;
        let cx = self.session.cx.clone();
        let input = &mut self.input;
        let last_result = &mut self.last_result;
        let mut failure = None;
        let receive = self.session.step(&mut |route, bytes| {
            let kind = bytes.get(6..8);
            let t = ClientInstant(now(&cx).map_err(|e| {
                failure = Some(Error::Session(e));
            })?);
            let event = if route == Route::Stream(inbound)
                && kind == Some(&(Kind::Challenge as u16).to_be_bytes())
            {
                match input.accept_control_challenge(bytes, t) {
                    Ok(()) => return Ok(Disposition::Consumed),
                    Err(presentation::Error::Input(input::Error::Control(
                        fr_client::authority::Error::Backpressure,
                    ))) => return Ok(Disposition::Blocked),
                    Err(e) => Err(e),
                }
            } else if route == Route::Stream(feedback) {
                if kind == Some(&(Kind::InputTicket as u16).to_be_bytes()) {
                    match input.accept_ticket(bytes, sample, t) {
                        Ok(()) | Err(presentation::Error::Input(input::Error::TicketExpired)) => {
                            return Ok(Disposition::Consumed);
                        }
                        Err(e) => Err(e),
                    }
                } else {
                    match input.result(bytes, t) {
                        Ok(event) => {
                            *last_result = Some(event);
                            result(event);
                            return Ok(Disposition::Consumed);
                        }
                        Err(e) => Err(e),
                    }
                }
            } else {
                let disposition = other(route, bytes)?;
                if disposition == Disposition::Consumed
                    && route == Route::Stream(progress)
                    && kind == Some(&(Kind::Progress as u16).to_be_bytes())
                {
                    match input.progress(bytes, &limits, t) {
                        Ok(())
                        | Err(presentation::Error::Media(fr_media::freshness::Error::Obsolete)) => {
                            Ok(())
                        }
                        Err(e) => Err(e),
                    }
                } else {
                    return Ok(disposition);
                }
            };
            event.map(|()| Disposition::Consumed).map_err(|e| {
                failure = Some(Error::View(e));
            })
        });
        if let Some(e) = failure {
            return Err(e);
        }
        receive?;
        self.send()?;
        self.check_inner().map(|_| ())
    }
    /// Run during idle as well as packet/UI activity. Observation responses,
    /// measured clock refresh, control replies, input and receipts share the
    /// original connection. `other` must admit media to bounded existing owners,
    /// never block on a codec. Progress becomes input evidence ONLY if that
    /// receiver accepted the exact progress record (Consumed, not Blocked).
    pub fn drive<'a>(
        &'a mut self,
        wait: Duration,
        mut result: impl FnMut(ResultEvent) + 'a,
        mut other: impl FnMut(Route, &[u8]) -> Result<Disposition, ()> + 'a,
    ) -> impl Future<Output = Result<(), Error>> + 'a {
        let operation = Operation {
            viewer: self,
            complete: false,
        };
        async move {
            let mut operation = operation;
            if wait > Duration::from_millis(100) {
                return Err(Error::Session(super::Error::InvalidConfiguration));
            }
            let t = operation.viewer.check_inner()?;
            // A submitted frame is not yet a visible frame. Pause all network
            // submission during this short callback gap, but do not make it
            // impossible for the actual visibility callback to complete. The
            // preceding view and every queued action keep their old deadlines.
            if !operation.viewer.input.tick(t)? {
                let viewer = &mut *operation.viewer;
                let until = viewer.input.maintenance_deadline(t)?.0;
                let remaining = until
                    .checked_sub(now(&viewer.session.cx)?)
                    .ok_or(Error::Expired)?;
                let delay = wait.min(Duration::from_micros(remaining));
                if !delay.is_zero() {
                    asupersync::time::sleep(viewer.session.cx.now(), delay).await;
                }
                viewer.check_inner()?;
                operation.complete = true;
                return Ok(());
            }
            operation.viewer.step(&mut result, &mut other)?;
            let viewer = &mut *operation.viewer;
            let cx = &viewer.session.cx;
            let t = ClientInstant(now(cx)?);
            let mut until = viewer
                .input
                .view_deadline(t)?
                .0
                .min(viewer.session.heard_until);
            if let Some(p) = &viewer.pending {
                until = until.min(p.until);
            }
            if let Some(d) = viewer.input.control_response_deadline() {
                until = until.min(d.0);
            }
            if let Some(d) = viewer.session.responder.response_deadline() {
                until = until.min(d.0);
            }
            let remaining = until.checked_sub(now(cx)?).ok_or(Error::Expired)?;
            viewer
                .session
                .transport
                .drive(cx, wait.min(Duration::from_micros(remaining)), || {
                    permitted(&mut viewer.input, &viewer.control, cx)
                })
                .await
                .map_err(|e| Error::Session(e.into()))?;
            viewer.step(&mut result, &mut other)?;
            operation.complete = true;
            Ok(())
        }
    }
}
fn permitted(input: &mut PresentedInput, control: &ViewerControl, cx: &Cx) -> bool {
    !control.is_stopped() && now(cx).is_ok_and(|t| input.view_deadline(ClientInstant(t)).is_ok())
}
impl Drop for ControlledViewer {
    fn drop(&mut self) {
        self.close();
    }
}
struct Operation<'a> {
    viewer: &'a mut ControlledViewer,
    complete: bool,
}
impl Drop for Operation<'_> {
    fn drop(&mut self) {
        if !self.complete {
            self.viewer.close();
        }
    }
}

#[cfg(test)]
mod tests;
