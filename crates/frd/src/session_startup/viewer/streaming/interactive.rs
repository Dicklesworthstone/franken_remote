//! Observe continuously, then start one explicit control request at the user's
//! decision time. The original decoder, view history and connection never move
//! to another session; an observation-only negotiation is never upgraded.
use super::{
    Error, Guarded, Operation, Peer, Presentation, StreamingViewer,
    acquisition::{self, Dispatch, State},
};
use crate::{
    input_quic::NegotiatedInput,
    media::{PresentationReceipt, PresentationStage, presented::ViewSample},
    session_startup::viewer::{ViewerSession, controlled::Error as ControlError, now},
};
use asupersync::{cx::Cx, types::CancelKind};
use fr_client::{
    control_grant::RequestControl,
    input::{ClientInstant, Policy, ResultEvent},
};
use fr_media::{
    delivery::ReceivePipeline,
    freshness::{ViewEvidence, ViewTracker},
};
use fr_transport::quic::{Disposition, Route};
use fr_wire::{Kind, control::Request};
use std::{future::Future, time::Duration};

/// The opt-in interactive callback keeps legacy immediate-control callbacks
/// source compatible. Viewing carries no grant or implicit approval decision.
pub enum InteractiveState<'a> {
    Viewing(&'a mut ViewingControl),
    Requesting(&'a mut super::PendingControl),
    Controlled(&'a mut crate::session_startup::ControlledViewer),
}
/// One original receiver's evidence, moved unchanged into the requested grant.
/// No clone, synthesized source stamp, new visibility callback or mapping reset.
#[derive(Default)]
pub(super) struct LocalView {
    pub(super) view: Option<ViewTracker>,
    initial: Option<PresentationReceipt>,
    pub(super) latest: Option<Presentation>,
    pub(super) mapped: bool,
}
impl LocalView {
    pub(super) fn sample(&mut self, at: u64) -> Result<ViewSample, Error> {
        let Some(view) = &mut self.view else {
            return Ok(ViewSample::Pending);
        };
        ViewSample::from_view(view.presented_sample(at), at).map_err(Error::Freshness)
    }
    pub(super) fn decoded(&mut self, receipt: PresentationReceipt, at: u64) -> Result<(), Error> {
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
                receipt.stage == PresentationStage::SubmittedToCompositor,
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
        policy: Policy,
    ) -> Result<bool, Error> {
        let sample = session
            .clock_correlation()
            .map_err(|e| Error::Control(ControlError::Clock(e)))?;
        let Some(sample) = sample else {
            return Ok(false);
        };
        let at = now(&session.cx).map_err(Error::Session)?;
        if let Some(view) = &mut self.view {
            if view.clock_correlation() != sample {
                view.synchronize(sample, at).map_err(Error::Freshness)?;
            }
        } else {
            self.view = Some(
                ViewTracker::new(receiver, sample, policy.view_age_us, at)
                    .map_err(Error::Freshness)?,
            );
        }
        self.view
            .as_mut()
            .ok_or(Error::Closed)?
            .observe_receiver(receiver, at)
            .map_err(Error::Freshness)?;
        if let Some(initial) = self.initial.take() {
            self.decoded(initial, at)?;
        }
        Ok(true)
    }
}

/// Local UI capability for one explicitly control-capable viewing session.
/// Merely viewing, confirming coordinates, or reporting visibility sends no
/// control request, creates no input owner and reserves no native input Seat.
/// `request_control` starts exactly one request, when the user chooses control.
/// Focus loss, hiding, suspend or disconnect must call `stop`; no focus-gain
/// event may reuse this owner or automatically reacquire control.
pub struct ViewingControl {
    pub(super) cx: Cx,
    pub(super) request: Request,
    pub(super) channels: NegotiatedInput,
    pub(super) policy: Policy,
    pub(super) local: LocalView,
    pub(super) requested: Option<RequestControl>,
}
impl ViewingControl {
    fn new(
        session: &ViewerSession,
        channels: NegotiatedInput,
        request: Request,
        policy: Policy,
    ) -> Result<Self, Error> {
        acquisition::validate(session, &channels, request)?;
        Ok(Self {
            cx: session.cx.clone(),
            request,
            channels,
            policy,
            local: LocalView::default(),
            requested: None,
        })
    }
    /// Immutable selected target and intent, not a grant or wire request.
    pub const fn request(&self) -> Request {
        self.request
    }
    pub const fn presentation(&self) -> Option<Presentation> {
        self.local.latest
    }
    pub fn confirm_mapping(
        &mut self,
        parent: fr_wire::negotiation::ControlBinding,
        view: fr_core::input::InputView,
    ) -> Result<(), Error> {
        now(&self.cx).map_err(Error::Session)?;
        if parent != self.request.parent || view != self.request.target.view {
            return Err(Error::Control(ControlError::WrongBinding));
        }
        self.local.mapped = true;
        Ok(())
    }
    /// Only the independent platform visibility witness may confirm this frame.
    /// Its original presentation deadline still applies, even before requesting.
    pub fn visible(&mut self, frame: u64) -> Result<ViewEvidence, Error> {
        let at = now(&self.cx).map_err(Error::Session)?;
        self.local
            .view
            .as_mut()
            .ok_or(Error::Control(ControlError::ClockNotReady))?
            .visible(frame, at)
            .map_err(Error::Freshness)
    }
    /// Inspect actual current evidence without creating or refreshing it. No
    /// evidence (including stale/unknown source) grants control on its own.
    pub fn evidence(&mut self) -> Result<ViewEvidence, Error> {
        let at = now(&self.cx).map_err(Error::Session)?;
        self.local
            .view
            .as_mut()
            .ok_or(Error::Control(ControlError::ClockNotReady))?
            .evidence(at)
            .map_err(Error::Freshness)
    }
    /// Called once for a real local user decision. Construct the existing wire
    /// owner NOW, not on the next network poll. A duplicate call is refused and
    /// cannot extend the first deadline. Success only stages a request: host
    /// consent, native initialization, mapping and visibility remain mandatory.
    pub fn request_control(&mut self) -> Result<ClientInstant, Error> {
        let at = ClientInstant(now(&self.cx).map_err(Error::Session)?);
        if self.requested.is_some() {
            return Err(Error::RequestAlreadyStarted);
        }
        let pending = RequestControl::new(
            self.request,
            self.channels.channel_binding(),
            self.channels.limits(),
            at,
        )
        .map_err(|e| Error::Control(ControlError::ControlRequest(e)))?;
        let deadline = pending.deadline();
        self.requested = Some(pending);
        Ok(deadline)
    }
    pub fn stop(&self) {
        self.cx.cancel_fast(CancelKind::User);
    }
    pub(super) fn prepare(
        &mut self,
        session: &mut ViewerSession,
        receiver: &ReceivePipeline,
    ) -> Result<bool, Error> {
        acquisition::validate(session, &self.channels, self.request)?;
        self.local.prepare(session, receiver, self.policy)
    }
    pub(super) fn decoded(&mut self, receipt: PresentationReceipt) -> Result<(), Error> {
        self.local
            .decoded(receipt, now(&self.cx).map_err(Error::Session)?)
    }
    pub(super) fn sample(&mut self) -> Result<ViewSample, Error> {
        self.local.sample(now(&self.cx).map_err(Error::Session)?)
    }
    pub(super) async fn drive(
        &mut self,
        session: &mut ViewerSession,
        wait: Duration,
        mut other: impl FnMut(Route, &[u8]) -> Result<Disposition, ()>,
    ) -> Result<(), Error> {
        acquisition::validate(session, &self.channels, self.request)?;
        let mut unsolicited = false;
        let result = session
            .drive(wait, |route, bytes| {
                if bytes.get(6..8) == Some(&(Kind::LeaseGranted as u16).to_be_bytes()) {
                    unsolicited = true;
                    Err(())
                } else {
                    other(route, bytes)
                }
            })
            .await;
        if unsolicited {
            return Err(Error::Control(ControlError::WrongBinding));
        }
        result.map_err(Error::Session)
    }
}
impl StreamingViewer {
    /// Keep watching until the UI explicitly requests control on `InteractiveState::Viewing`.
    /// Observation renewal, clocks, source proof, repairs and decoding continue
    /// before that decision without consuming a control-request budget. The
    /// decision starts the unchanged request deadline; promotion transfers the
    /// exact existing presentation history. No retry or automatic reacquisition.
    /// Call on a fresh control-capable streaming owner, not by dropping `serve`.
    /// Dropping this future, even unpolled, still closes the original session.
    pub fn serve_interactive_control<'a>(
        &'a mut self,
        channels: NegotiatedInput,
        request: Request,
        policy: Policy,
        mut ui: impl FnMut(InteractiveState<'_>, Option<Presentation>) -> Result<(), ()> + 'a,
        mut result: impl FnMut(ResultEvent) + 'a,
        mut other: impl FnMut(Route, &[u8]) -> Result<Disposition, ()> + 'a,
    ) -> impl Future<Output = Result<(), Error>> + 'a {
        let viewing = match &self.peer {
            Peer::Observe { session, .. } if !self.served => {
                ViewingControl::new(session, channels, request, policy)
            }
            _ => Err(Error::Closed),
        };
        let fence = self.control.clone();
        let operation = Operation { viewer: self };
        Guarded {
            fence,
            inner: Box::pin(async move {
                let operation = operation;
                let mut viewing = viewing?;
                if let Some(initial) = operation.viewer.initial.take() {
                    viewing.decoded(initial)?;
                }
                let Peer::Observe { session, media } =
                    std::mem::replace(&mut operation.viewer.peer, Peer::Closed)
                else {
                    return Err(Error::Closed);
                };
                let (_, binding) = viewing
                    .channels
                    .viewer_scope(&session.transport)
                    .map_err(|e| Error::Control(ControlError::Input(e)))?;
                if binding != media.binding() {
                    return Err(Error::Control(ControlError::WrongBinding));
                }
                operation.viewer.peer = Peer::Viewing {
                    session,
                    media,
                    viewing: Box::new(viewing),
                };
                operation
                    .viewer
                    .serve_inner(
                        &mut |state, event| {
                            let state = match state {
                                Dispatch::Viewing(view) => InteractiveState::Viewing(view),
                                Dispatch::Existing(State::Requesting(pending)) => {
                                    InteractiveState::Requesting(pending)
                                }
                                Dispatch::Existing(State::Controlled(input)) => {
                                    InteractiveState::Controlled(input)
                                }
                                Dispatch::Existing(State::Observing) => return Err(()),
                            };
                            ui(state, event)
                        },
                        &mut result,
                        &mut other,
                    )
                    .await
            }),
        }
    }
}
