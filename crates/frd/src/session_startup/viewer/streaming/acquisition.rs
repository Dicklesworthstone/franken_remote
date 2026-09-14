//! Live observation-to-control handoff. Decoder work never owns the request,
//! original connection, or cancellation gate. All wire state uses `RequestControl`.
use super::{
    Error, Guarded, Operation, Peer, Presentation, StreamingViewer, StreamingViewerControl,
};
use crate::{
    input_quic::NegotiatedInput,
    media::PresentationReceipt,
    session_startup::viewer::{
        ViewerSession,
        controlled::{ControlledViewer, Error as ControlError},
        now,
    },
};
use asupersync::cx::Cx;
use fr_client::{
    control_grant::RequestControl,
    input::{ClientInstant, InputClient, Policy, ResultEvent, presentation::PresentedInput},
};
use fr_media::{
    delivery::ReceivePipeline,
    freshness::{ClockCorrelation, ViewEvidence, ViewTracker},
};
use fr_transport::quic::{self, Disposition, Route};
use fr_wire::{
    Channel, Kind,
    control::{Granted, Request},
    negotiation::Role,
};
use std::{future::Future, time::Duration};

/// Before the real grant there is no input owner. The platform can acknowledge
/// only its actual coordinate mapping and independent visibility. Never infer a
/// visibility acknowledgement from the accompanying compositor-submission event.
pub enum State<'a> {
    Observing,
    Requesting(&'a mut PendingControl),
    Controlled(&'a mut ControlledViewer),
}
impl<'a> State<'a> {
    pub(super) fn controlled(self) -> Option<&'a mut ControlledViewer> {
        match self {
            Self::Controlled(viewer) => Some(viewer),
            _ => None,
        }
    }
}
/// One explicit, non-replayable request and the original media-derived evidence.
/// No field exposes a mutable connection, a fabricated clock or input credentials.
pub struct PendingControl {
    cx: Cx,
    request: Request,
    pending: RequestControl,
    channels: NegotiatedInput,
    policy: Policy,
    view: Option<ViewTracker>,
    initial: Option<PresentationReceipt>,
    latest: Option<Presentation>,
    mapped: bool,
    grant: Option<(Granted, InputClient)>,
}
fn input_error(error: fr_client::input::Error) -> Error {
    Error::Control(ControlError::View(
        fr_client::input::presentation::Error::Input(error),
    ))
}
impl PendingControl {
    fn new(
        session: &ViewerSession,
        channels: NegotiatedInput,
        request: Request,
        policy: Policy,
    ) -> Result<Self, Error> {
        // This function runs at public method CALL time, outside the async body.
        let pending = RequestControl::new(
            request,
            channels.channel_binding(),
            session.opened.selection.limits,
            ClientInstant(now(&session.cx).map_err(Error::Session)?),
        )
        .map_err(|e| Error::Control(ControlError::ControlRequest(e)))?;
        if session.opened.selection.role != Role::RequestControl
            || !session
                .opened
                .selection
                .capabilities
                .iter()
                .any(|c| c.name == crate::input_quic::grant::CAPABILITY && c.version == 1)
        {
            return Err(Error::Control(ControlError::ControlNotNegotiated));
        }
        if session.clock.is_none() {
            return Err(Error::Control(ControlError::ClockNotReady));
        }
        if request.parent != session.opened.binding
            || channels.limits() != session.opened.selection.limits
        {
            return Err(Error::Control(ControlError::WrongBinding));
        }
        channels
            .check_request(&session.transport, request)
            .map_err(|e| Error::Control(ControlError::Input(e)))?;
        channels
            .viewer_routes(&session.transport)
            .map_err(|e| Error::Control(ControlError::Input(e)))?;
        Ok(Self {
            cx: session.cx.clone(),
            request,
            pending,
            channels,
            policy,
            view: None,
            initial: None,
            latest: None,
            mapped: false,
            grant: None,
        })
    }
    fn current(&self) -> Result<u64, Error> {
        let at = now(&self.cx).map_err(Error::Session)?;
        if at >= self.pending.deadline().0
            || self
                .grant
                .as_ref()
                .is_some_and(|(_, input)| input.ticket_deadline().is_none_or(|d| at >= d.0))
        {
            return Err(Error::Control(ControlError::Expired));
        }
        Ok(at)
    }
    /// The precise requested host target. This is not permission to send input.
    pub const fn request(&self) -> Request {
        self.request
    }
    /// A real native completion, including the startup token when provided.
    /// Its stage does not certify visibility; delayed visibility still expires.
    pub const fn presentation(&self) -> Option<Presentation> {
        self.latest
    }
    pub fn deadline(&self) -> ClientInstant {
        self.pending.deadline()
    }
    /// Confirm only after the platform has installed this exact coordinate map.
    pub fn confirm_mapping(
        &mut self,
        parent: fr_wire::negotiation::ControlBinding,
        view: fr_core::input::InputView,
    ) -> Result<(), Error> {
        self.current()?;
        if parent != self.request.parent || view != self.request.target.view {
            return Err(Error::Control(ControlError::WrongBinding));
        }
        self.mapped = true;
        Ok(())
    }
    /// Platform-qualified visibility of the exact native completion, not decode
    /// completion, a heartbeat or receipt of the host's grant. Callbacks cannot
    /// revive a previous receiver or extend a picture's original queue deadline.
    pub fn visible(&mut self, frame: u64) -> Result<ViewEvidence, Error> {
        let at = self.current()?;
        self.view
            .as_mut()
            .ok_or(Error::Control(ControlError::ClockNotReady))?
            .visible(frame, at)
            .map_err(Error::Freshness)
    }
    /// Independently usable terminal cancellation. It never requests control anew.
    pub fn stop(&self) {
        self.cx.cancel_fast(asupersync::types::CancelKind::User);
    }
    pub(super) fn presented_sample(
        &mut self,
    ) -> Result<crate::media::presented::ViewSample, Error> {
        let at = self.current()?;
        let Some(view) = &mut self.view else {
            return Ok(crate::media::presented::ViewSample::Pending);
        };
        crate::media::presented::ViewSample::from_view(view.presented_sample(at), at)
            .map_err(Error::Freshness)
    }
    pub(super) fn decoded(&mut self, receipt: PresentationReceipt) -> Result<(), Error> {
        let at = self.current()?;
        if receipt.frame.as_raw() != receipt.decoded.descriptor().frame {
            return Err(Error::Control(ControlError::WrongBinding));
        }
        let event = Presentation {
            frame: receipt.frame,
            stage: receipt.stage,
        };
        if let Some(view) = &mut self.view {
            view.decoded(
                receipt.decoded,
                receipt.stage == crate::media::PresentationStage::SubmittedToCompositor,
                at,
            )
            .map_err(Error::Freshness)?;
        } else {
            self.initial = Some(receipt);
        }
        self.latest = Some(event);
        Ok(())
    }
    pub(super) fn prepare(
        &mut self,
        session: &mut ViewerSession,
        receiver: &ReceivePipeline,
    ) -> Result<bool, Error> {
        self.current()?;
        let sample = session
            .clock_correlation()
            .map_err(|e| Error::Control(ControlError::Clock(e)))?;
        let Some(sample) = sample else {
            return Ok(false);
        };
        let at = self.current()?;
        if let Some(view) = &mut self.view {
            if view.clock_correlation() != sample {
                view.synchronize(sample, at).map_err(Error::Freshness)?;
            }
        } else {
            self.view = Some(
                ViewTracker::new(receiver, sample, self.policy.view_age_us, at)
                    .map_err(Error::Freshness)?,
            );
        }
        self.view
            .as_mut()
            .unwrap()
            .observe_receiver(receiver, at)
            .map_err(Error::Freshness)?;
        if let Some(initial) = self.initial.take() {
            self.decoded(initial)?;
        }
        Ok(true)
    }
    fn service(&mut self, session: &mut ViewerSession) -> Result<Option<ClockCorrelation>, Error> {
        session.check().map_err(Error::Session)?;
        self.channels
            .check_request(&session.transport, self.request)
            .map_err(|e| Error::Control(ControlError::Input(e)))?;
        let sample = session
            .clock_correlation()
            .map_err(|e| Error::Control(ControlError::Clock(e)))?;
        let at = self.current()?;
        let until = self.pending.deadline().0;
        if let Some((_, input)) = &mut self.grant {
            input.tick(ClientInstant(at)).map_err(input_error)?;
        } else if sample.is_some()
            && let Some(bytes) = self
                .pending
                .pending(ClientInstant(at))
                .map_err(|e| Error::Control(ControlError::ControlRequest(e)))?
        {
            match session.transport.send(
                &self.cx,
                Route::Stream(session.routes.outbound),
                bytes,
                until,
                || now(&self.cx).is_ok_and(|n| n < until),
            ) {
                Ok(()) => self
                    .pending
                    .sent(ClientInstant(now(&self.cx).map_err(Error::Session)?))
                    .map_err(|e| Error::Control(ControlError::ControlRequest(e)))?,
                Err(quic::Error::Backpressure) => {}
                Err(error) => return Err(Error::Transport(error)),
            }
        }
        Ok(sample)
    }
    pub(super) async fn drive(
        &mut self,
        session: &mut ViewerSession,
        media: &crate::media_quic::NegotiatedMedia,
        wait: Duration,
        mut other: impl FnMut(Route, &[u8]) -> Result<Disposition, ()>,
    ) -> Result<(), Error> {
        let sample = self.service(session)?;
        let inbound = Route::Stream(session.routes.inbound);
        let routes = media
            .viewer_routes(&session.transport)
            .map_err(Error::Routes)?;
        let at = self.current()?;
        let until = self.pending.deadline().0;
        let mut failure = None;
        let result = session
            .drive(
                wait.min(Duration::from_micros(until - at)),
                |route, bytes| {
                    let handled = (|| {
                        if bytes.get(6..8) == Some(&(Kind::LeaseGranted as u16).to_be_bytes()) {
                            if route != inbound || self.grant.is_some() {
                                return Err(Error::Control(ControlError::WrongBinding));
                            }
                            let at = self.current()?;
                            let sample =
                                sample.ok_or(Error::Control(ControlError::ClockNotReady))?;
                            self.grant = Some(
                                self.pending
                                    .accept(bytes, sample, self.policy, ClientInstant(at))
                                    .map_err(|e| Error::Control(ControlError::ControlRequest(e)))?,
                            );
                            return Ok(Disposition::Consumed);
                        }
                        let disposition = other(route, bytes).map_err(|()| Error::Application)?;
                        if disposition == Disposition::Consumed
                            && routes
                                .iter()
                                .any(|(r, channel)| *r == route && *channel == Channel::MediaConfig)
                            && bytes.get(6..8) == Some(&(Kind::Progress as u16).to_be_bytes())
                            && let Some(view) = &mut self.view
                        {
                            match view.progress(
                                bytes,
                                &media.limits(),
                                now(&self.cx).map_err(Error::Session)?,
                            ) {
                                Ok(()) | Err(fr_media::freshness::Error::Obsolete) => {}
                                Err(error) => return Err(Error::Freshness(error)),
                            }
                        }
                        Ok(disposition)
                    })();
                    handled.map_err(|error| {
                        failure = Some(error);
                    })
                },
            )
            .await;
        if let Some(error) = failure {
            return Err(error);
        }
        result.map_err(Error::Session)?;
        self.service(session)?;
        Ok(())
    }
}
impl StreamingViewer {
    /// Request a real grant while the SAME decoder, receiver, repair lane and
    /// observation/clock services continue. Call instead of `serve` after actual
    /// startup; `RequestControl` and input-channel attachment must be negotiated.
    /// The UI receives no input owner until both the grant and local prerequisites
    /// are ready. All deadlines start at this call, including time before polling.
    /// Error or drop (even unpolled) closes the original viewer before codec work.
    pub fn serve_requesting_control<'a>(
        &'a mut self,
        channels: NegotiatedInput,
        request: Request,
        policy: Policy,
        mut ui: impl FnMut(State<'_>, Option<Presentation>) -> Result<(), ()> + 'a,
        mut result: impl FnMut(ResultEvent) + 'a,
        mut other: impl FnMut(Route, &[u8]) -> Result<Disposition, ()> + 'a,
    ) -> impl Future<Output = Result<(), Error>> + 'a {
        let pending = match &self.peer {
            Peer::Observe { session, .. } if !self.served => {
                PendingControl::new(session, channels, request, policy)
            }
            _ => Err(Error::Closed),
        };
        let fence = self.control.clone();
        let operation = Operation { viewer: self };
        Guarded {
            fence,
            inner: Box::pin(async move {
                let operation = operation;
                let mut pending = pending?;
                if let Some(initial) = operation.viewer.initial.take() {
                    pending.decoded(initial)?;
                }
                let Peer::Observe { session, media } =
                    std::mem::replace(&mut operation.viewer.peer, Peer::Closed)
                else {
                    return Err(Error::Closed);
                };
                let (_, binding) = pending
                    .channels
                    .viewer_scope(&session.transport)
                    .map_err(|e| Error::Control(ControlError::Input(e)))?;
                if binding != media.binding() {
                    return Err(Error::Control(ControlError::WrongBinding));
                }
                operation.viewer.peer = Peer::Acquiring {
                    session,
                    media,
                    request: Box::new(pending),
                };
                operation
                    .viewer
                    .serve_inner(&mut ui, &mut result, &mut other)
                    .await
            }),
        }
    }
}
pub(super) fn notify(
    peer: &mut Peer,
    receiver: &ReceivePipeline,
    control: &mut StreamingViewerControl,
    event: Option<Presentation>,
    ui: &mut impl FnMut(State<'_>, Option<Presentation>) -> Result<(), ()>,
) -> Result<(), Error> {
    if let Peer::Acquiring {
        session, request, ..
    } = peer
    {
        request.prepare(session, receiver)?;
    }
    let state = match peer {
        Peer::Acquiring { request, .. } => State::Requesting(request),
        Peer::Control(viewer) => State::Controlled(viewer),
        Peer::Observe { .. } => State::Observing,
        Peer::Closed => return Err(Error::Closed),
    };
    ui(state, event).map_err(|()| Error::Application)?;
    let ready = if let Peer::Acquiring {
        session, request, ..
    } = peer
    {
        request.prepare(session, receiver)?;
        request.current()?;
        if request.grant.is_some()
            && request.mapped
            && let Some(view) = &mut request.view
        {
            match view.evidence(now(&request.cx).map_err(Error::Session)?) {
                Ok(_) => true,
                Err(
                    fr_media::freshness::Error::NotSubmitted
                    | fr_media::freshness::Error::SourceUnknown
                    | fr_media::freshness::Error::SourceStale,
                ) => false,
                Err(error) => return Err(Error::Freshness(error)),
            }
        } else {
            false
        }
    } else {
        false
    };
    if ready {
        let Peer::Acquiring {
            session,
            media,
            mut request,
        } = std::mem::replace(peer, Peer::Closed)
        else {
            return Err(Error::Closed);
        };
        let at = ClientInstant(request.current()?);
        let (_, input) = request.grant.take().ok_or(Error::Closed)?;
        let view = request.view.take().ok_or(Error::Closed)?;
        let mut input = PresentedInput::from_view(input, receiver, view, at)
            .map_err(|e| Error::Control(ControlError::View(e)))?;
        input
            .confirm_mapping(
                request.request.parent.remote_session,
                request.request.target.view,
                at,
            )
            .map_err(|e| Error::Control(ControlError::View(e)))?;
        let viewer = session
            .into_controlled_synchronized(request.channels, media, input)
            .map_err(Error::Control)?;
        control.input = Some(viewer.control());
        *peer = Peer::Control(viewer);
    }
    Ok(())
}

#[cfg(test)]
mod tests;
