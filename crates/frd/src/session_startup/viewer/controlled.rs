//! The running native viewer owns input identities, freshness and network sends.
//! Decode/present work stays off this task; qualified callbacks arrive between turns.
mod clipboard;
mod disconnect;
pub mod events;
mod files;
pub mod local_cursor;
pub mod request;
mod viewport;
use super::{ViewerSession, now};
use crate::{
    input_quic::{self, NegotiatedInput},
    media::clock::{self, ClockSync},
    media_quic::NegotiatedMedia,
};
use asupersync::{cx::Cx, types::CancelKind};
use fr_client::input::{
    self, Action, ClientInstant, Encoded, ResultEvent, StopReason, Unsent,
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
    Clipboard(crate::clipboard_quic::Error),
    Files(fr_files::sender::Error),
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
    ControlRequest(fr_client::control_grant::Error),
    ControlNotNegotiated,
    /// The exact authenticated host report, not an inferred cleanup result.
    LeaseRevoked(fr_wire::lease_revoked::Revoked),
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
        self.stopped.load(Ordering::Acquire) || self.cx.is_cancel_requested()
    }
}
struct Pending {
    bytes: [u8; MAX_INPUT_RECORD_BYTES],
    len: usize,
    until: u64,
    /// The sequence this never-admitted record consumed, returned if a stale
    /// view suspends input before the transport admits it.
    unsent: Unsent,
}
/// Consumes one existing explicit grant and its exact media/input/clock owners.
/// One fixed-size pending record holds an action, pointer OR held-state snapshot.
/// Subsequent UI events are backpressured BEFORE consuming another identity.
pub struct ControlledViewer {
    // These owners contain fixed protocol buffers and replay ledgers. Keep
    // them at stable addresses across promotion/async results instead of copying
    // their full inline storage through every nested Poll/Result on the stack.
    session: Box<ViewerSession>,
    input: Box<PresentedInput>,
    viewport: fr_client::input::viewport::Viewport,
    channels: NegotiatedInput,
    /// Absent only while a reference recovery holds the media (plan 12.3):
    /// input is then suspended, and its renewals stay serviced without media.
    media: Option<NegotiatedMedia>,
    clock: ClockSync,
    clock_at: u64,
    pending: Option<Pending>,
    control: ViewerControl,
    last_result: Option<ResultEvent>,
    disconnect_outcome: Option<fr_transport::ControlCloseOutcome>,
    events: Option<events::Receiver>,
    native_capture: Option<Box<dyn events::NativeCapture>>,
    clipboard: Option<crate::clipboard_quic::Bridge>,
    clipboard_setup: crate::session_startup::clipboard::Setup,
    files: Box<files::Slot>,
    /// The platform owner of the local pointer image (remote cursor, §11.4).
    local_cursor: Option<Box<dyn local_cursor::LocalCursor>>,
    /// Positions this viewer encoded for the host, to tell lag from divergence.
    pointer_history: Box<fr_client::cursor::owner::PointerHistory>,
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
        // Negotiated reference recovery: a lost reference suspends this grant
        // until the recovered generation instead of ending it (plan 12.3).
        if self.opened.selection.capabilities.iter().any(|c| {
            c.name == fr_wire::recovery_request::CAPABILITY
                && c.version == fr_wire::recovery_request::VERSION
        }) {
            input.expect_reference_recovery();
        }
        let control = ViewerControl {
            cx: self.cx.clone(),
            stopped: Arc::new(AtomicBool::new(false)),
        };
        let viewport = input.viewport();
        Ok(ControlledViewer {
            session: Box::new(self),
            input: Box::new(input),
            viewport,
            channels,
            media: Some(media),
            clock,
            clock_at: sample.received_at_us(),
            pending: None,
            control,
            last_result: None,
            disconnect_outcome: None,
            events: None,
            native_capture: None,
            clipboard: None,
            clipboard_setup: crate::session_startup::clipboard::Setup::default(),
            files: Box::default(),
            local_cursor: None,
            pointer_history: Box::default(),
        })
    }
}
impl ControlledViewer {
    pub(super) fn presented_sample(
        &mut self,
    ) -> Result<crate::media::presented::ViewSample, Error> {
        let at = self.check()?;
        let sample = match self.input.presented_sample(at) {
            Ok(sample) => Ok(sample),
            Err(presentation::Error::Media(error)) => Err(error),
            Err(error) => return Err(Error::View(error)),
        };
        crate::media::presented::ViewSample::from_view(sample, at.0)
            .map_err(|e| Error::View(presentation::Error::Media(e)))
    }
    pub(super) fn streaming_parts(
        &mut self,
    ) -> Result<(&mut ViewerSession, &NegotiatedMedia), Error> {
        self.check()?;
        Ok((&mut self.session, self.media.as_ref().ok_or(Error::Closed)?))
    }
    /// The parent session a reference recovery keeps serving through this
    /// grant's own drive (renewals, tickets, results, clock).
    pub(super) fn session_mut(&mut self) -> &mut ViewerSession {
        &mut self.session
    }
    /// A reference loss was found: suspend this grant's input now and stop
    /// consulting the fenced view until the recovered generation is installed.
    pub(super) fn enter_recovery(&mut self) -> Result<(), Error> {
        let t = ClientInstant(now(&self.session.cx)?);
        self.input.suspend_for_recovery(t)?;
        self.abandon_if_suspended()
    }
    /// Hand this stream's media to its reference recovery, suspending the
    /// grant's input first: the lost reference means the view cannot advance
    /// (plan 12.3). Renewals continue; no input is sent until resumed.
    pub(super) fn take_media_for_recovery(&mut self) -> Result<NegotiatedMedia, Error> {
        let t = self.check()?;
        self.input.suspend_for_recovery(t)?;
        self.abandon_if_suspended()?;
        self.media.take().ok_or(Error::Closed)
    }
    /// Install the recovered media and move this suspended grant to the
    /// recovered receiver's generation. Input resumes only on its fresh
    /// evidence and a ticket naming it.
    pub(super) fn install_recovered_media(
        &mut self,
        media: NegotiatedMedia,
        receiver: &fr_media::delivery::ReceivePipeline,
    ) -> Result<(), Error> {
        if self.media.is_some() {
            return Err(Error::Closed);
        }
        media.check(&self.session.transport).map_err(Error::Media)?;
        let t = ClientInstant(now(&self.session.cx)?);
        self.input.follow_recovery(receiver, t)?;
        let b = media.binding();
        let v = self.input.input_view();
        if v.recovery != b.recovery || v.configuration != b.configuration {
            return Err(Error::WrongBinding);
        }
        self.media = Some(media);
        Ok(())
    }
    pub(super) fn check_stream_receiver(
        &self,
        receiver: &fr_media::delivery::ReceivePipeline,
    ) -> Result<(), Error> {
        self.input.check_receiver(receiver).map_err(Error::View)
    }
    /// The host's exact grant (it never clamps a request): what this viewer
    /// asked for after negotiation, e.g. without line scrolling on a host
    /// whose input executor cannot scroll by lines.
    pub fn granted_capabilities(&self) -> fr_core::input_submission::Capabilities {
        self.input.capabilities()
    }
    /// Stale-view suspensions of this grant's input so far and their total
    /// duration in microseconds, the current one included (plan 21: time
    /// spent with input suspended by stale view). Counts only, no content.
    pub fn suspension_totals(&self) -> (u32, u64) {
        let at = now(&self.session.cx).unwrap_or(0);
        self.input.suspension_totals(ClientInstant(at))
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
        self.fence_input();
        self.session.close();
    }
    fn check_inner(&mut self) -> Result<ClientInstant, Error> {
        if self.is_closed() {
            return Err(Error::Closed);
        }
        self.files.check_pending().map_err(Error::Files)?;
        self.session.check()?;
        self.channels
            .viewer_routes(&self.session.transport)
            .map_err(Error::Input)?;
        if let Some(media) = &self.media {
            media.check(&self.session.transport).map_err(Error::Media)?;
        }
        let t = ClientInstant(now(&self.session.cx)?);
        self.input.maintenance_deadline(t)?;
        self.abandon_if_suspended()?;
        if let Some(events) = &self.events {
            events.check(t).map_err(Error::Capture)?;
        }
        if self.pending.as_ref().is_some_and(|p| t.0 >= p.until) {
            return Err(Error::Expired);
        }
        Ok(t)
    }
    /// A stale view suspended input (plan 11.3): the one record the transport
    /// never admitted is returned to the input owner, never sent late and never
    /// expired as a failure. The host never saw it.
    fn abandon_if_suspended(&mut self) -> Result<(), Error> {
        if self.input.suspended_since().is_some()
            && let Some(pending) = self.pending.take()
        {
            self.input
                .abandon_unsent(pending.unsent)
                .map_err(Error::View)?;
        }
        Ok(())
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
            unsent: Unsent::Pointer,
        };
        let result = self.input.action(action, &mut p.bytes, t)?;
        p.len = result.bytes;
        p.unsent = Unsent::Action(result.sequence);
        self.pending = Some(p);
        self.note_action(&action, t);
        Ok(result)
    }
    pub fn pointer(&mut self, position: DesktopPoint) -> Result<Encoded, Error> {
        let t = self.slot()?;
        let until = self.input.send_deadline(t)?.0;
        let mut p = Pending {
            bytes: [0; MAX_INPUT_RECORD_BYTES],
            len: 0,
            until,
            unsent: Unsent::Pointer,
        };
        let result = self.input.pointer(position, &mut p.bytes, t)?;
        p.len = result.bytes;
        self.pending = Some(p);
        self.note_pointer(position, t);
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
            unsent: Unsent::Pointer,
        };
        let Some(result) = self.input.reconcile_held(observed, &mut p.bytes, t)? else {
            return Ok(None);
        };
        p.len = result.bytes;
        p.unsent = Unsent::Held(result.sequence);
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
                let view = std::cell::Cell::new(None);
                match q.send(
                    cx,
                    Route::Stream(self.session.routes.outbound),
                    &bytes,
                    until.0,
                    || gate(&mut self.input, &self.control, cx, &view),
                ) {
                    Ok(()) => self.input.control_response_sent(ClientInstant(now(cx)?))?,
                    Err(quic::Error::Backpressure) => {}
                    Err(e) => return Err(gated(e, view.take())),
                }
            }
        }
        if let Some(p) = &self.pending {
            let view = std::cell::Cell::new(None);
            match self.channels.send(cx, q, &p.bytes[..p.len], p.until, || {
                gate(&mut self.input, &self.control, cx, &view)
            }) {
                Ok(()) => self.pending = None,
                Err(input_quic::Error::Transport(quic::Error::Backpressure)) => {}
                // Suspended between the check and this send: admission refused
                // it (all or nothing), so it was never admitted either.
                Err(_) if self.input.suspended_since().is_some() && !self.control.is_stopped() => {
                    self.abandon_if_suspended()?;
                }
                Err(e) => {
                    return Err(match (e, view.take()) {
                        (input_quic::Error::Transport(quic::Error::Unauthorized), Some(view)) => {
                            Error::View(view)
                        }
                        (e, _) => Error::Input(e),
                    });
                }
            }
        }
        Ok(())
    }
    #[allow(clippy::too_many_lines)]
    fn step(
        &mut self,
        result: &mut impl FnMut(ResultEvent),
        other: &mut impl FnMut(Route, &[u8]) -> Result<Disposition, ()>,
    ) -> Result<(), Error> {
        self.service_clipboard()?;
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
        // During a reference recovery there is no media: its progress belongs
        // to the recovery's own dispatch, never to this grant's evidence.
        let progress = self
            .media
            .as_ref()
            .map(|media| {
                media
                    .progress_route(&self.session.transport)
                    .map(|route| (route, media.limits()))
            })
            .transpose()
            .map_err(Error::Media)?;
        let inbound = self.session.routes.inbound;
        let control_binding = fr_wire::authority::Binding {
            channel: inbound.binding,
            session: self.session.opened.binding.remote_session,
        };
        let cx = self.session.cx.clone();
        let files = &self.files;
        let clipboard = &self.clipboard;
        let clipboard_setup = &self.clipboard_setup;
        let input = &mut self.input;
        let last_result = &mut self.last_result;
        let mut failure = None;
        let receive = self.session.step(&mut |route, bytes| {
            if files.owns(route, bytes)
                || clipboard.as_ref().is_some_and(|c| c.owns_inbound(route))
                || clipboard_setup.owns(route, bytes)
            {
                return Ok(Disposition::Blocked);
            }
            let kind = bytes.get(6..8);
            if route == Route::Stream(inbound)
                && kind == Some(&(Kind::LeaseRevoked as u16).to_be_bytes())
            {
                // Fence immediately, before another record/captured action or
                // renewal send. Operation's existing teardown stops native
                // capture and drops unsent bytes without erasing input receipts.
                failure = Some(match input.accept_lease_revoked(bytes, control_binding) {
                    Ok(revoked) => Error::LeaseRevoked(revoked),
                    Err(error) => Error::View(error),
                });
                return Err(());
            }
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
                if let Some((progress, limits)) = progress
                    && disposition == Disposition::Consumed
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
        self.service_clipboard()?;
        self.send()?;
        // One bounded bulk turn follows input, clock and authority servicing.
        self.service_files()?;
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
            // Check the file setup horizon without sampling the input clock.
            // Clipboard permission/service advances that same input clock, so
            // tick's timestamp must be obtained AFTER clipboard servicing.
            operation
                .viewer
                .files
                .check_pending()
                .map_err(Error::Files)?;
            operation.viewer.service_clipboard()?;
            let t = operation.viewer.check_inner()?;
            // The source-age bound follows this path's measured round trip.
            let rtt = operation.viewer.session.transport.smoothed_rtt_us();
            operation
                .viewer
                .input
                .follow_path_rtt(rtt)
                .map_err(Error::View)?;
            // A submitted frame is not yet a visible frame. Pause all network
            // submission during this short callback gap, but do not make it
            // impossible for the actual visibility callback to complete. The
            // preceding view and every queued action keep their old deadlines.
            // A SUSPENDED view (plan 11.3) instead keeps the session's I/O
            // running below: fresh media, reports and renewals resume it.
            if !operation.viewer.input.tick(t)?
                && operation.viewer.input.suspended_since().is_none()
            {
                let viewer = &mut *operation.viewer;
                let mut until = viewer.input.maintenance_deadline(t)?.0;
                if let Some(deadline) = viewer.files.deadline_us() {
                    until = until.min(deadline);
                }
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
            let mut until = input_deadline(&mut viewer.input, t)?
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
            if let Some(deadline) = viewer.files.deadline_us() {
                until = until.min(deadline);
            }
            if let Some(deadline) = viewer.clipboard_setup.deadline_us() {
                until = until.min(deadline);
            }
            let current = now(cx)?;
            let Some(remaining) = until.checked_sub(current) else {
                return Err(expired(viewer, current));
            };
            let view = std::cell::Cell::new(None);
            viewer
                .session
                .transport
                .drive(cx, wait.min(Duration::from_micros(remaining)), || {
                    viewer.files.permits_io()
                        && viewer
                            .clipboard
                            .as_ref()
                            .is_none_or(crate::clipboard_quic::Bridge::permits_io)
                        && viewer.clipboard_setup.permits_io()
                        && gate(&mut viewer.input, &viewer.control, cx, &view)
                })
                .await
                .map_err(|e| gated(e, view.take()))?;
            viewer.step(&mut result, &mut other)?;
            operation.complete = true;
            Ok(())
        }
    }
}
fn permitted(input: &mut PresentedInput, control: &ViewerControl, cx: &Cx) -> bool {
    gate(input, control, cx, &std::cell::Cell::new(None))
}
/// The view gate records WHY it refused, so a stale view that the transport
/// meets first is not reported as a bare authorization failure.
fn gate(
    input: &mut PresentedInput,
    control: &ViewerControl,
    cx: &Cx,
    view: &std::cell::Cell<Option<presentation::Error>>,
) -> bool {
    if control.is_stopped() {
        return false;
    }
    let Ok(t) = now(cx) else {
        return false;
    };
    match input.view_deadline(ClientInstant(t)) {
        Ok(_) => true,
        // Suspended by a stale view (plan 11.3): the session's own I/O (media,
        // presented reports, renewals) continues. No input record is sent:
        // new ones are refused and the unsent one is abandoned, never admitted.
        Err(_) if input.suspended_since().is_some() && input.stopped().is_none() => true,
        Err(error) => {
            view.set(Some(error));
            false
        }
    }
}
/// The earliest deadline the input owner imposes on this turn: its view
/// deadline, or while suspended by a stale view, the suspension limit.
fn input_deadline(input: &mut PresentedInput, t: ClientInstant) -> Result<ClientInstant, Error> {
    match input.view_deadline(t) {
        Ok(until) => Ok(until),
        Err(_) if input.suspended_since().is_some() && input.stopped().is_none() => {
            input.maintenance_deadline(t).map_err(Error::View)
        }
        Err(error) => Err(Error::View(error)),
    }
}
/// Which local deadline had already passed: the view (its owner reports why),
/// the session's host-silence bound, else a pending record or response.
fn expired(viewer: &mut ControlledViewer, current: u64) -> Error {
    if let Err(view) = viewer.input.view_deadline(ClientInstant(current)) {
        return Error::View(view);
    }
    if current >= viewer.session.heard_until {
        return Error::Session(super::Error::Expired);
    }
    Error::Expired
}
/// Only this viewer's own view-gate refusal is relabelled. The transport checks
/// its identity/lifetime gate first, so an identity refusal leaves `view` empty.
fn gated(error: quic::Error, view: Option<presentation::Error>) -> Error {
    match (error, view) {
        (quic::Error::Unauthorized, Some(view)) => Error::View(view),
        (error, _) => Error::Session(error.into()),
    }
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

#[test]
fn inline_viewer_owners_fit_composed_native_service_stacks() {
    // A full native suite on ordinary test-thread stacks exposed aborts while
    // moving the inline session/input owners through nested async completions.
    assert!(std::mem::size_of::<ControlledViewer>() <= 16 * 1024);
    assert!(std::mem::size_of::<super::streaming::StreamingViewer>() <= 20 * 1024);
}
