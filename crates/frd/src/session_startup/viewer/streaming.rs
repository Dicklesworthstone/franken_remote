//! Continuous native receiving without lending the connection to a codec.
//! One owned picture stays charged while QUIC, repairs and control keep moving.
use super::{
    ViewerSession,
    controlled::{ControlledViewer, ViewerControl},
    now,
};
use crate::{
    media::receiver_feedback::{self as feedback, Setup, ViewerFeedback},
    media::{self, PresentationStage, Presenter, decoder_startup},
    media_quic::NegotiatedMedia,
    worker::Deadline,
};
use asupersync::{cx::Cx, types::CancelKind};
use fr_client::input::ResultEvent;
use fr_media::{
    access_unit::FrameId,
    delivery::{BudgetUsage, DeliveryError, ReceivePipeline},
};
use fr_transport::quic::{self, Disposition, Route};
use std::{
    future::{Future, poll_fn},
    pin::{Pin, pin},
    task::{Context, Poll},
    time::Duration,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Session(super::Error),
    Control(super::controlled::Error),
    Startup(decoder_startup::Error),
    Media(media::Error),
    Routes(crate::media_quic::Error),
    Delivery(DeliveryError),
    Transport(quic::Error),
    Feedback(feedback::Error),
    Application,
    Closed,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Error {}

/// A native completion, never a promise of visibility or scanout. The platform
/// may call `ControlledViewer::visible` only after its own qualified evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Presentation {
    pub frame: FrameId,
    pub stage: PresentationStage,
}
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Statistics {
    pub decoded: u64,
    pub compositor_submissions: u64,
    pub repair_requests: u64,
    pub network_turns: u64,
}
/// Independently usable terminal stop; it cannot replace observation or acquire
/// input. A native decoder never owns or blocks this handle.
#[derive(Clone)]
pub struct StreamingViewerControl {
    cx: Cx,
    input: Option<ViewerControl>,
}
impl StreamingViewerControl {
    pub fn stop(&self) {
        if let Some(input) = &self.input {
            input.stop();
        }
        self.cx.cancel_fast(CancelKind::User);
    }
}

#[allow(clippy::large_enum_variant)]
enum Peer {
    Observe {
        session: ViewerSession,
        media: NegotiatedMedia,
    },
    Control(ControlledViewer),
}
impl Peer {
    fn parts(&mut self) -> Result<(&mut ViewerSession, &NegotiatedMedia), Error> {
        match self {
            Self::Observe { session, media } => {
                session.check().map_err(Error::Session)?;
                media.check(&session.transport).map_err(Error::Routes)?;
                Ok((session, media))
            }
            Self::Control(v) => v.streaming_parts().map_err(Error::Control),
        }
    }
    fn controlled(&mut self) -> Option<&mut ControlledViewer> {
        match self {
            Self::Control(v) => Some(v),
            Self::Observe { .. } => None,
        }
    }
    fn close(&mut self) {
        match self {
            Self::Control(v) => v.close(),
            Self::Observe { session, .. } => session.close(),
        }
    }
    async fn drive(
        &mut self,
        wait: Duration,
        result: &mut impl FnMut(ResultEvent),
        other: impl FnMut(Route, &[u8]) -> Result<Disposition, ()>,
    ) -> Result<(), Error> {
        match self {
            Self::Observe { session, .. } => {
                session.drive(wait, other).await.map_err(Error::Session)
            }
            Self::Control(v) => v.drive(wait, result, other).await.map_err(Error::Control),
        }
    }
}

/// One actual startup's persistent decoder, receiver and original QUIC session.
/// Encoded memory remains charged across native awaits. No decoder thread, task
/// or additional queue is spawned, and UI callbacks remain bounded/nonblocking.
pub struct StreamingViewer {
    peer: Peer,
    presenter: Presenter,
    receiver: ReceivePipeline,
    repair: Repair,
    feedback: Option<ViewerFeedback>,
    control: StreamingViewerControl,
    statistics: Statistics,
    served: bool,
}
impl ViewerSession {
    pub fn into_streaming(
        self,
        media: NegotiatedMedia,
        startup: decoder_startup::Viewer,
    ) -> Result<StreamingViewer, Error> {
        let (presenter, receiver) = startup
            .finish_stream(&self.transport, &media)
            .map_err(Error::Startup)?;
        StreamingViewer::new(
            Peer::Observe {
                session: self,
                media,
            },
            presenter,
            receiver,
        )
    }
}
impl ControlledViewer {
    /// Input must already be granted and joined to THIS startup's receiver.
    /// There is no synthetic initial decoded/visible frame or implicit grant.
    pub fn into_streaming(
        self,
        presenter: Presenter,
        receiver: ReceivePipeline,
    ) -> Result<StreamingViewer, Error> {
        StreamingViewer::new(Peer::Control(self), presenter, receiver)
    }
}
impl StreamingViewer {
    fn new(
        mut peer: Peer,
        mut presenter: Presenter,
        mut receiver: ReceivePipeline,
    ) -> Result<Self, Error> {
        let (session, media) = match peer.parts() {
            Ok(parts) => parts,
            Err(error) => {
                peer.close();
                receiver.close();
                presenter.abort();
                return Err(error);
            }
        };
        let cx = session.cx.clone();
        if let Err(error) = presenter.check_stream(&session.transport, media, &receiver) {
            peer.close();
            receiver.close();
            presenter.abort();
            return Err(Error::Media(error));
        }
        if let Some(v) = peer.controlled()
            && let Err(error) = v.check_stream_receiver(&receiver)
        {
            peer.close();
            receiver.close();
            presenter.abort();
            return Err(Error::Control(error));
        }
        let setup = (|| {
            let (session, media) = peer.parts()?;
            let setup = Setup::selected(
                &session.opened.selection,
                session.opened.binding,
                media.binding(),
            )
            .map_err(Error::Feedback)?;
            setup
                .map(|s| {
                    ViewerFeedback::new(
                        s,
                        Route::Stream(session.routes.inbound),
                        Route::Stream(session.routes.outbound),
                    )
                })
                .transpose()
                .map_err(Error::Feedback)
        })();
        let feedback = match setup {
            Ok(feedback) => feedback,
            Err(error) => {
                peer.close();
                receiver.close();
                presenter.abort();
                return Err(error);
            }
        };
        let input = peer.controlled().map(|v| v.control());
        Ok(Self {
            peer,
            presenter,
            receiver,
            repair: Repair::default(),
            feedback,
            control: StreamingViewerControl { cx, input },
            statistics: Statistics::default(),
            served: false,
        })
    }
    pub fn control(&self) -> StreamingViewerControl {
        self.control.clone()
    }
    pub fn worker_id(&self) -> Option<u32> {
        self.presenter.worker_id()
    }
    pub fn statistics(&self) -> Statistics {
        self.statistics
    }
    /// Actually admitted advisory reports; these are not display acknowledgements.
    pub fn receiver_feedback_reports(&self) -> u64 {
        self.feedback.as_ref().map_or(0, |f| f.sent)
    }
    pub fn budget_usage(&self) -> BudgetUsage {
        self.receiver.budget_usage()
    }
    pub fn close(&mut self) {
        // Fence before codec cancellation or memory retirement.
        self.control.stop();
        self.peer.close();
        self.receiver.close();
        self.presenter.abort();
        self.repair.clear();
    }
    /// Reaping is a separate observed OS result, not implied by cancellation.
    pub async fn reap_media(
        &mut self,
        cleanup: &Cx,
        deadline: Deadline,
    ) -> Result<asupersync::process::ExitStatus, media::Error> {
        self.close();
        self.presenter.reap(cleanup, deadline).await
    }
    /// Retain authentic input results after closure without replaying effects.
    pub fn last_result(&mut self) -> Option<ResultEvent> {
        self.peer.controlled().and_then(|v| v.last_result())
    }
    /// Run until stop, expiry or failure. Between bounded network turns `ui`
    /// receives optional native completion metadata and the existing input owner.
    /// It can submit actions or confirm independently observed visibility. It
    /// MUST NOT wait for platform IO. `other` handles non-media application lanes.
    /// Even dropping an unpolled future is a terminal cancellation, and fences
    /// authority before any in-flight foreign operation is abandoned.
    pub fn serve<'a>(
        &'a mut self,
        mut ui: impl FnMut(Option<&mut ControlledViewer>, Option<Presentation>) -> Result<(), ()> + 'a,
        mut result: impl FnMut(ResultEvent) + 'a,
        mut other: impl FnMut(Route, &[u8]) -> Result<Disposition, ()> + 'a,
    ) -> impl Future<Output = Result<(), Error>> + 'a {
        let fence = self.control.clone();
        let operation = Operation { viewer: self };
        Guarded {
            fence,
            inner: Box::pin(async move {
                let operation = operation;
                operation
                    .viewer
                    .serve_inner(&mut ui, &mut result, &mut other)
                    .await
            }),
        }
    }
    async fn serve_inner(
        &mut self,
        ui: &mut impl FnMut(Option<&mut ControlledViewer>, Option<Presentation>) -> Result<(), ()>,
        result: &mut impl FnMut(ResultEvent),
        other: &mut impl FnMut(Route, &[u8]) -> Result<Disposition, ()>,
    ) -> Result<(), Error> {
        if self.served {
            return Err(Error::Closed);
        }
        self.served = true;
        let cx = &self.control.cx;
        ui(self.peer.controlled(), None).map_err(|()| Error::Application)?;
        loop {
            let job = self
                .presenter
                .take_next(cx, &mut self.receiver)
                .map_err(Error::Media)?;
            let Some(job) = job else {
                network(
                    &mut self.peer,
                    &mut self.receiver,
                    &mut self.repair,
                    &mut self.statistics,
                    self.feedback.as_mut(),
                    cx,
                    result,
                    other,
                )
                .await?;
                ui(self.peer.controlled(), None).map_err(|()| Error::Application)?;
                continue;
            };
            if let Some(f) = &mut self.feedback {
                f.begin(now(cx).map_err(Error::Session)?)
                    .map_err(Error::Feedback)?;
            }
            let job = {
                let mut decoding = pin!(self.presenter.decode_job(cx, job));
                loop {
                    let decoded = {
                        let turn = network(
                            &mut self.peer,
                            &mut self.receiver,
                            &mut self.repair,
                            &mut self.statistics,
                            self.feedback.as_mut(),
                            cx,
                            result,
                            other,
                        );
                        let mut turn = pin!(turn);
                        let mut decoded = None;
                        // Do not cancel a healthy canonical network drive just
                        // because native completion won the race. Finish its turn.
                        poll_fn(|task| {
                            if decoded.is_none()
                                && let Poll::Ready(r) = decoding.as_mut().poll(task)
                            {
                                decoded = Some(r);
                            }
                            if let Some(Err(e)) = &decoded {
                                self.control.stop();
                                return Poll::Ready(Err(Error::Media(*e)));
                            }
                            turn.as_mut().poll(task)
                        })
                        .await?;
                        decoded
                    };
                    if let Some(stage) = decoded {
                        break stage.map_err(Error::Media)?;
                    }
                    ui(self.peer.controlled(), None).map_err(|()| Error::Application)?;
                }
            };
            let receipt = job.complete(cx, &mut self.receiver).map_err(Error::Media)?;
            if let Some(f) = &mut self.feedback {
                f.complete(now(cx).map_err(Error::Session)?)
                    .map_err(Error::Feedback)?;
            }
            let stage = receipt.stage;
            let event = Presentation {
                frame: receipt.frame,
                stage,
            };
            if let Some(viewer) = self.peer.controlled() {
                viewer
                    .decoded(
                        receipt.decoded,
                        stage == PresentationStage::SubmittedToCompositor,
                    )
                    .map_err(Error::Control)?;
            }
            self.statistics.decoded = self.statistics.decoded.saturating_add(1);
            if stage == PresentationStage::SubmittedToCompositor {
                self.statistics.compositor_submissions =
                    self.statistics.compositor_submissions.saturating_add(1);
            }
            // Completion releases the original decode reservation only after native borrowing.
            ui(self.peer.controlled(), Some(event)).map_err(|()| Error::Application)?;
        }
    }
}
impl Drop for StreamingViewer {
    fn drop(&mut self) {
        self.close();
    }
}
struct Operation<'a> {
    viewer: &'a mut StreamingViewer,
}
impl Drop for Operation<'_> {
    fn drop(&mut self) {
        self.viewer.close();
    }
}
struct Guarded<F> {
    fence: StreamingViewerControl,
    inner: Pin<Box<F>>,
}
impl<F: Future> Future for Guarded<F> {
    type Output = F::Output;
    fn poll(self: Pin<&mut Self>, task: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        match this.inner.as_mut().poll(task) {
            Poll::Ready(v) => {
                this.fence.stop();
                Poll::Ready(v)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}
impl<F> Drop for Guarded<F> {
    fn drop(&mut self) {
        self.fence.stop();
    }
}

// Repair wire grammar admits at most 64 ranges, irrespective of AU size. One
// fixed pending buffer avoids both retained-record growth and allocation churn.
struct Repair {
    bytes: [u8; 36 + 64 * 8],
    len: usize,
    until: u64,
    frame: u64,
}
impl Default for Repair {
    fn default() -> Self {
        Self {
            bytes: [0; 36 + 64 * 8],
            len: 0,
            until: 0,
            frame: 0,
        }
    }
}
impl Repair {
    fn clear(&mut self) {
        self.bytes.fill(0);
        self.len = 0;
        self.until = 0;
        self.frame = 0;
    }
    fn prepare(&mut self, receiver: &mut ReceivePipeline, current: u64) -> Result<(), Error> {
        receiver.tick(current).map_err(Error::Delivery)?;
        if self.len != 0 && !receiver.repair_needed(self.frame) {
            self.clear();
        }
        if self.len != 0 && current >= self.until {
            return Err(Error::Closed);
        }
        if self.len == 0
            && let Some(offer) = receiver
                .repair_offer(current, &mut self.bytes)
                .map_err(Error::Delivery)?
        {
            self.len = offer.bytes;
            self.frame = offer.frame;
            self.until = offer
                .reference_deadline_us
                .min(current.checked_add(80_000).ok_or(Error::Closed)?);
        }
        Ok(())
    }
}
#[allow(clippy::too_many_arguments)]
async fn network(
    peer: &mut Peer,
    receiver: &mut ReceivePipeline,
    repair: &mut Repair,
    statistics: &mut Statistics,
    mut feedback: Option<&mut ViewerFeedback>,
    cx: &Cx,
    result: &mut impl FnMut(ResultEvent),
    other: &mut impl FnMut(Route, &[u8]) -> Result<Disposition, ()>,
) -> Result<(), Error> {
    let (session, media) = peer.parts()?;
    let routes = media
        .viewer_routes(&session.transport)
        .map_err(Error::Routes)?;
    repair.prepare(receiver, now(cx).map_err(Error::Session)?)?;
    if repair.len != 0 {
        let route = Route::Stream(
            media
                .repair_stream(&session.transport)
                .map_err(Error::Routes)?,
        );
        match session
            .transport
            .send(cx, route, &repair.bytes[..repair.len], repair.until, || {
                now(cx).is_ok_and(|n| n < repair.until) && receiver.repair_needed(repair.frame)
            }) {
            Ok(()) => {
                repair.clear();
                statistics.repair_requests = statistics.repair_requests.saturating_add(1);
            }
            Err(quic::Error::Backpressure) => {}
            Err(e) => return Err(Error::Transport(e)),
        }
    }
    let mut failure = None;
    let driven = peer
        .drive(Duration::from_millis(5), result, |route, bytes| {
            if feedback::is_feedback(bytes) {
                let current = now(cx).map_err(|e| {
                    failure = Some(Error::Session(e));
                })?;
                let f = feedback.as_mut().ok_or_else(|| {
                    failure = Some(Error::Feedback(feedback::Error::Binding));
                })?;
                f.receive(route, bytes, receiver, current).map_err(|e| {
                    failure = Some(Error::Feedback(e));
                })?;
                Ok(Disposition::Consumed)
            } else if let Some((_, channel)) = routes.iter().find(|(r, _)| *r == route) {
                let current = now(cx).map_err(|e| {
                    failure = Some(Error::Session(e));
                })?;
                receiver.receive(*channel, bytes, current).map_err(|e| {
                    failure = Some(Error::Delivery(e));
                })?;
                Ok(Disposition::Consumed)
            } else {
                other(route, bytes)
            }
        })
        .await;
    if let Some(error) = failure {
        return Err(error);
    }
    driven?;
    receiver
        .tick(now(cx).map_err(Error::Session)?)
        .map_err(Error::Delivery)?;
    // The session's renewal, repair and input services get their turn first.
    if let Some(feedback) = feedback {
        let (session, _) = peer.parts()?;
        feedback
            .service(&mut session.transport, cx, now(cx).map_err(Error::Session)?)
            .map_err(Error::Feedback)?;
    }
    statistics.network_turns = statistics.network_turns.saturating_add(1);
    Ok(())
}

#[cfg(test)]
mod tests;
