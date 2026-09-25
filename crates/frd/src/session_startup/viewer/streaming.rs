//! Continuous native receiving without lending the connection to a codec.
//! One owned picture stays charged while QUIC, repairs and control keep moving.
mod acquisition;
pub(crate) mod audio;
mod continuation;
mod cursor;
mod interactive;
mod recovery;
use super::{
    ViewerSession,
    controlled::{ControlledViewer, ViewerControl},
    now,
};
use crate::{
    media::presented::{self as presented, ViewSample, ViewerPresentation},
    media::receiver_feedback::{self as feedback, Setup, ViewerFeedback},
    media::{self, PresentationStage, Presenter, decoder_startup},
    media_quic::{NegotiatedMedia, recovery as recovery_control},
    worker::Deadline,
};
pub use acquisition::{PendingControl, State as ControlState};
use asupersync::{cx::Cx, types::CancelKind};
use fr_client::input::ResultEvent;
use fr_media::{
    access_unit::FrameId,
    delivery::{BudgetUsage, DeliveryError, ReceivePipeline},
};
use fr_transport::quic::{self, Disposition, Route};
pub use interactive::{InteractiveState, ViewingControl};
use std::{
    future::{Future, poll_fn},
    pin::{Pin, pin},
    task::{Context, Poll},
    time::Duration,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Session(super::Error),
    Clipboard(crate::clipboard_quic::Error),
    Control(super::controlled::Error),
    Startup(decoder_startup::Error),
    Media(media::Error),
    Routes(crate::media_quic::Error),
    Delivery(DeliveryError),
    Recovery(recovery_control::Error),
    Replacement(crate::media_quic::replacement::Error),
    Transport(quic::Error),
    Feedback(feedback::Error),
    PresentedState(presented::Error),
    Freshness(fr_media::freshness::Error),
    /// A malformed or hostile remote cursor record (typed, no pixels).
    Cursor(cursor::Fault),
    Wire(fr_wire::WireError),
    Application,
    RequestAlreadyStarted,
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
    /// Completed recovery handshakes, not visibility or renewed control grants.
    pub recovered_streams: u64,
}
impl Statistics {
    fn presented(&mut self, stage: PresentationStage) {
        self.decoded = self.decoded.saturating_add(1);
        if stage == PresentationStage::SubmittedToCompositor {
            self.compositor_submissions = self.compositor_submissions.saturating_add(1);
        }
    }
}
/// Independently usable terminal stop; it cannot replace observation or acquire
/// input. A native decoder never owns or blocks this handle.
#[derive(Clone)]
pub struct StreamingViewerControl {
    cx: Cx,
    input: Option<ViewerControl>,
}
impl StreamingViewerControl {
    pub(super) fn observing(cx: Cx) -> Self {
        Self { cx, input: None }
    }
    /// Terminal cancellation of the ORIGINAL session, including a handle retained
    /// before decoder startup or input promotion. This never grants authority.
    pub fn is_stopped(&self) -> bool {
        self.cx.is_cancel_requested() || self.input.as_ref().is_some_and(ViewerControl::is_stopped)
    }
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
    Control(Box<ControlledViewer>),
    Viewing {
        session: ViewerSession,
        media: NegotiatedMedia,
        viewing: Box<ViewingControl>,
    },
    Acquiring {
        session: ViewerSession,
        media: NegotiatedMedia,
        request: Box<PendingControl>,
    },
    Closed,
}
impl Peer {
    fn prepare_decode(&mut self, receiver: &ReceivePipeline) -> Result<bool, Error> {
        match self {
            Self::Acquiring {
                session, request, ..
            } => request.prepare(session, receiver),
            Self::Viewing {
                session, viewing, ..
            } => viewing.prepare(session, receiver),
            Self::Closed => Err(Error::Closed),
            _ => Ok(true),
        }
    }
    fn presented_sample(&mut self, receiver: &ReceivePipeline) -> Result<ViewSample, Error> {
        match self {
            Self::Acquiring {
                session, request, ..
            } => {
                request.prepare(session, receiver)?;
                request.presented_sample()
            }
            Self::Viewing {
                session, viewing, ..
            } => {
                viewing.prepare(session, receiver)?;
                viewing.sample()
            }
            Self::Control(viewer) => viewer.presented_sample().map_err(Error::Control),
            Self::Observe { .. } => Ok(ViewSample::Pending),
            Self::Closed => Err(Error::Closed),
        }
    }
    fn decoded(&mut self, receipt: media::PresentationReceipt) -> Result<(), Error> {
        match self {
            Self::Control(viewer) => viewer
                .decoded(
                    receipt.decoded,
                    receipt.stage == PresentationStage::SubmittedToCompositor,
                )
                .map_err(Error::Control),
            Self::Acquiring { request, .. } => request.decoded(receipt),
            Self::Viewing { viewing, .. } => viewing.decoded(receipt),
            Self::Observe { .. } => Ok(()),
            Self::Closed => Err(Error::Closed),
        }
    }
    fn parts(&mut self) -> Result<(&mut ViewerSession, &NegotiatedMedia), Error> {
        match self {
            Self::Observe { session, media }
            | Self::Acquiring { session, media, .. }
            | Self::Viewing { session, media, .. } => {
                session.check().map_err(Error::Session)?;
                media.check(&session.transport).map_err(Error::Routes)?;
                Ok((session, media))
            }
            Self::Control(v) => v.streaming_parts().map_err(Error::Control),
            Self::Closed => Err(Error::Closed),
        }
    }
    /// Parent-only service during a failed observation. Retired media cannot
    /// suppress original renewal, clocks or advisory reply cleanup.
    fn parent(&mut self) -> Result<&mut ViewerSession, Error> {
        match self {
            Self::Observe { session, .. } => {
                session.check().map_err(Error::Session)?;
                Ok(session)
            }
            _ => self.parts().map(|(session, _)| session),
        }
    }
    fn controlled(&mut self) -> Option<&mut ControlledViewer> {
        match self {
            Self::Control(v) => Some(v.as_mut()),
            Self::Observe { .. } | Self::Acquiring { .. } | Self::Viewing { .. } | Self::Closed => {
                None
            }
        }
    }
    fn close(&mut self) {
        match self {
            Self::Control(v) => v.close(),
            Self::Observe { session, .. }
            | Self::Acquiring { session, .. }
            | Self::Viewing { session, .. } => session.close(),
            Self::Closed => {}
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
            Self::Viewing {
                session, viewing, ..
            } => viewing.drive(session, wait, other).await,
            Self::Acquiring {
                session,
                media,
                request,
            } => request.drive(session, media, wait, other).await,
            Self::Closed => Err(Error::Closed),
        }
    }
}

/// One actual startup's persistent decoder, receiver and original QUIC session.
/// Encoded memory remains charged across native awaits. No decoder thread, task
/// or additional queue is spawned, and UI callbacks remain bounded/nonblocking.
pub struct StreamingViewer {
    clipboard: Option<crate::native_clipboard::Application>,
    peer: Peer,
    presenter: Presenter,
    receiver: ReceivePipeline,
    repair: Repair,
    recovery: Option<Box<recovery_control::Receiver>>,
    feedback: Option<ViewerFeedback>,
    presentation: Option<ViewerPresentation>,
    /// Present only when the host selected `remote-cursor`.
    cursor: Option<cursor::ViewerCursor>,
    /// Present only when this observer selected audio-down and it attached.
    audio: Option<audio::ViewerAudio>,
    control: StreamingViewerControl,
    statistics: Statistics,
    served: bool,
    initial: Option<media::PresentationReceipt>,
}
impl ViewerSession {
    /// Retain the actual first native completion for a subsequent control request.
    /// No decode, visibility, mapping or grant is synthesized by this handoff.
    /// The token keeps its receiver identity and original presentation deadline.
    pub fn into_streaming_presented(
        self,
        media: NegotiatedMedia,
        startup: decoder_startup::Viewer,
        initial: media::PresentationReceipt,
    ) -> Result<StreamingViewer, Error> {
        let mut viewer = self.into_streaming(media, startup)?;
        if let Some(recovery) = &mut viewer.recovery {
            recovery
                .observe_decoded(&initial.decoded)
                .map_err(Error::Recovery)?;
        }
        viewer.initial = Some(initial);
        Ok(viewer)
    }
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
        StreamingViewer::new(Peer::Control(Box::new(self)), presenter, receiver)
    }
}
impl StreamingViewer {
    #[cfg(test)]
    pub(in crate::session_startup) fn from_test_parts(
        session: ViewerSession,
        media: NegotiatedMedia,
        presenter: Presenter,
        receiver: ReceivePipeline,
        initial: media::PresentationReceipt,
    ) -> Self {
        let mut viewer = Self::new(Peer::Observe { session, media }, presenter, receiver).unwrap();
        if let Some(recovery) = &mut viewer.recovery {
            recovery.observe_decoded(&initial.decoded).unwrap();
        }
        viewer.initial = Some(initial);
        viewer
    }

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
            let feedback = setup
                .map(|s| {
                    ViewerFeedback::new(
                        s,
                        Route::Stream(session.routes.inbound),
                        Route::Stream(session.routes.outbound),
                    )
                })
                .transpose()
                .map_err(Error::Feedback)?;
            let presentation = ViewerPresentation::attach(
                &session.opened.selection,
                session.opened.binding,
                media.binding(),
                &session.transport,
                session.routes.outbound,
                now(&cx).map_err(Error::Session)?,
            )
            .map_err(Error::PresentedState)?;
            let recovery = if session.opened.selection.capabilities.iter().any(|cap| {
                cap.name == fr_wire::recovery_request::CAPABILITY
                    && cap.version == fr_wire::recovery_request::VERSION
            }) {
                Some(
                    media
                        .recovery_receiver(
                            &session.transport,
                            session.routes,
                            session.opened.binding,
                            &receiver,
                        )
                        .map_err(Error::Recovery)?,
                )
            } else {
                None
            };
            let cursor = cursor::ViewerCursor::attach(media, &session.transport)?;
            let audio = audio::ViewerAudio::attach(media, &session.transport)?;
            Ok((feedback, presentation, recovery, cursor, audio))
        })();
        let (feedback, presentation, recovery, cursor, audio) = match setup {
            Ok(owners) => owners,
            Err(error) => {
                peer.close();
                receiver.close();
                presenter.abort();
                return Err(error);
            }
        };
        let input = peer.controlled().map(|v| v.control());
        Ok(Self {
            clipboard: None,
            peer,
            presenter,
            receiver,
            repair: Repair::default(),
            recovery: recovery.map(Box::new),
            feedback,
            presentation,
            cursor,
            audio,
            control: StreamingViewerControl { cx, input },
            statistics: Statistics::default(),
            served: false,
            initial: None,
        })
    }
    /// Install the native client's local output for an ATTACHED audio-down
    /// lane, once, before serving. `Err(())`: audio was not selected/attached
    /// (typed absence) or an output is already installed.
    pub(crate) fn configure_audio(
        &mut self,
        output: Box<dyn audio::AudioOutput>,
    ) -> Result<(), audio::Unavailable> {
        if self.served {
            return Err(audio::Unavailable::AlreadyConfigured);
        }
        self.audio
            .as_mut()
            .ok_or(audio::Unavailable::NotSelected)?
            .configure(output)
    }
    pub(crate) fn audio_statistics(&self) -> Option<audio::AudioStatistics> {
        self.audio.as_ref().map(audio::ViewerAudio::statistics)
    }
    pub(crate) fn configure_clipboard(
        &mut self,
        config: crate::native_clipboard::Configuration,
    ) -> Result<crate::native_clipboard::Control, crate::clipboard_quic::Error> {
        use crate::clipboard_quic::Error as E;
        if self.served || self.clipboard.is_some() {
            return Err(E::AlreadyAttached);
        }
        let (session, _) = self.peer.parts().map_err(|_| E::Closed)?;
        session.check().map_err(|_| E::Closed)?;
        crate::session_startup::clipboard::selected(&session.opened.selection)?;
        let app = crate::native_clipboard::Application::new(config);
        let control = app.control();
        self.clipboard = Some(app);
        Ok(control)
    }
    pub(crate) async fn reap_clipboard(
        &mut self,
        cx: &Cx,
        deadline: Deadline,
    ) -> Result<crate::native_clipboard::Cleanup, crate::clipboard_quic::Error> {
        let Some(app) = &mut self.clipboard else {
            return Ok(crate::native_clipboard::Cleanup::NotStarted);
        };
        let result = app.reap(cx, deadline).await;
        if let Some(viewer) = self.peer.controlled() {
            app.collect_received(viewer)?;
        }
        result
    }
    pub(crate) fn collect_clipboard(&mut self) -> Result<(), crate::clipboard_quic::Error> {
        if let Some(app) = &mut self.clipboard
            && let Some(viewer) = self.peer.controlled()
        {
            app.collect_received(viewer)?;
        }
        Ok(())
    }
    pub fn control(&self) -> StreamingViewerControl {
        self.control.clone()
    }
    /// Actual original file results remain readable after service/connection
    /// closure. Reading a receipt never retries an uncertain publication.
    pub fn file_result(&mut self) -> Option<fr_files::sender::Receipt> {
        self.peer
            .controlled()
            .and_then(|viewer| viewer.file_result())
    }
    pub fn take_file_result(&mut self) -> Option<fr_files::sender::Receipt> {
        self.peer
            .controlled()
            .and_then(ControlledViewer::take_file_result)
    }
    /// Preserve every started file's receipt when a batch outlives the serving
    /// future. Reading or collecting these results never requires fresh authority.
    pub fn file_batch_report(&mut self) -> Option<fr_files::sender::batch::Report> {
        self.peer
            .controlled()
            .and_then(|viewer| viewer.file_batch_report())
    }
    pub fn take_file_batch_report(&mut self) -> Option<fr_files::sender::batch::Report> {
        self.peer
            .controlled()
            .and_then(ControlledViewer::take_file_batch_report)
    }
    /// Fence the parent at CALL time, then collect the original file source.
    /// Success proves either that no source remains or that its thread was
    /// joined. Expired/cancelled/abandoned waits keep its owner and receipt here.
    pub fn reap_files<'a>(
        &'a mut self,
        cleanup: &'a Cx,
        deadline: Deadline,
    ) -> impl Future<Output = Result<(), fr_files::sender::Error>> + 'a {
        self.close();
        async move {
            match self.peer.controlled() {
                Some(viewer) => viewer.reap_files(cleanup, deadline).await,
                None => Ok(()),
            }
        }
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
    /// Reports admitted to the original transport; not proof of host receipt or scanout.
    pub fn presentation_reports(&self) -> u64 {
        self.presentation.as_ref().map_or(0, |p| p.sent)
    }
    /// A request admitted to transport is not a healed decoder or a new grant.
    /// The original failure deadline remains active until session replacement.
    pub fn recovery_state(&self) -> Option<recovery_control::State> {
        self.recovery
            .as_deref()
            .map(recovery_control::Receiver::state)
    }
    pub fn budget_usage(&self) -> BudgetUsage {
        self.receiver.budget_usage()
    }
    pub fn close(&mut self) {
        // Fence before codec cancellation or memory retirement.
        self.control.stop();
        self.peer.close();
        if let Some(app) = &mut self.clipboard {
            app.close();
        }
        self.receiver.close();
        self.presenter.abort();
        self.repair.clear();
        if let Some(recovery) = &mut self.recovery {
            recovery.close();
        }
        self.initial = None;
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
    /// Reap the same native input producer after service exits, without exposing
    /// a second mutable control owner or waiting on a native thread.
    pub fn input_capture_cleanup(&mut self) -> super::controlled::events::CaptureCleanup {
        self.peer.controlled().map_or(
            super::controlled::events::CaptureCleanup::NotStarted,
            super::controlled::ControlledViewer::input_capture_cleanup,
        )
    }
    /// Fence at call time and retain the original native input owner until its
    /// nonblocking cleanup observation completes or the absolute budget expires.
    pub fn reap_input_capture<'a>(
        &'a mut self,
        cleanup: &'a Cx,
        deadline: Deadline,
    ) -> impl Future<
        Output = Result<
            super::controlled::events::CaptureCleanup,
            super::controlled::events::CaptureReapError,
        >,
    > + 'a {
        self.close();
        async move {
            match self.peer.controlled() {
                Some(viewer) => viewer.reap_input_capture(cleanup, deadline).await,
                None => Ok(super::controlled::events::CaptureCleanup::NotStarted),
            }
        }
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
                    .serve_inner(
                        &mut |state, event| ui(state.controlled(), event),
                        &mut result,
                        &mut other,
                    )
                    .await
            }),
        }
    }
    #[allow(clippy::too_many_lines)]
    async fn serve_inner(
        &mut self,
        ui: &mut impl FnMut(acquisition::Dispatch<'_>, Option<Presentation>) -> Result<(), ()>,
        result: &mut impl FnMut(ResultEvent),
        other: &mut impl FnMut(Route, &[u8]) -> Result<Disposition, ()>,
    ) -> Result<(), Error> {
        if self.served {
            return Err(Error::Closed);
        }
        self.served = true;
        if matches!(self.peer, Peer::Acquiring { .. } | Peer::Viewing { .. }) {
            let (session, _) = self.peer.parts()?;
            crate::native_clipboard::Application::decline_unconfigured(
                &mut self.clipboard,
                crate::session_startup::clipboard::selected(&session.opened.selection).is_ok(),
            );
        }
        let cx = &self.control.cx.clone();
        acquisition::notify(&mut self.peer, &self.receiver, &mut self.control, None, ui)?;
        loop {
            // Tick before selecting native work. A lost reference may expire
            // during silence, with no packet or decoder completion to wake it.
            let recovering = recovery::service(
                &mut self.peer,
                self.recovery.as_deref_mut(),
                &mut self.receiver,
                &mut self.repair,
                cx,
            )?;
            if recovering
                && matches!(self.peer, Peer::Observe { .. })
                && self.recovery_state() == Some(recovery_control::State::Requested)
            {
                let event = self.resume_observation(ui, other).await?;
                acquisition::notify(
                    &mut self.peer,
                    &self.receiver,
                    &mut self.control,
                    Some(event),
                    ui,
                )?;
                continue;
            }
            if !recovering {
                // Between decode jobs only: the presenter is not borrowed.
                self.apply_cursor(cx).await?;
            }
            let ready = !recovering && self.peer.prepare_decode(&self.receiver)?;
            let job = if ready {
                recovery::admit(
                    &mut self.peer,
                    self.recovery.as_deref_mut(),
                    &mut self.receiver,
                    &mut self.repair,
                    cx,
                    |receiver| self.presenter.take_next(cx, receiver),
                )?
                .flatten()
            } else {
                None
            };
            let Some(job) = job else {
                network(
                    &mut self.peer,
                    self.clipboard.as_mut(),
                    &mut self.receiver,
                    &mut self.repair,
                    self.recovery.as_deref_mut(),
                    &mut self.statistics,
                    self.feedback.as_mut(),
                    self.presentation.as_mut(),
                    self.cursor.as_mut(),
                    self.audio.as_mut(),
                    cx,
                    result,
                    other,
                )
                .await?;
                acquisition::notify(&mut self.peer, &self.receiver, &mut self.control, None, ui)?;
                continue;
            };
            if let Some(f) = &mut self.feedback {
                f.begin(now(cx).map_err(Error::Session)?)
                    .map_err(Error::Feedback)?;
            }
            let job = {
                let drain_observation = recovery::enabled(&self.peer, self.recovery.as_deref());
                let mut decoding =
                    pin!(self.presenter.decode_stream_job(cx, job, drain_observation));
                loop {
                    let decoded = {
                        let turn = network(
                            &mut self.peer,
                            self.clipboard.as_mut(),
                            &mut self.receiver,
                            &mut self.repair,
                            self.recovery.as_deref_mut(),
                            &mut self.statistics,
                            self.feedback.as_mut(),
                            self.presentation.as_mut(),
                            self.cursor.as_mut(),
                            self.audio.as_mut(),
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
                    acquisition::notify(
                        &mut self.peer,
                        &self.receiver,
                        &mut self.control,
                        None,
                        ui,
                    )?;
                }
            };
            let receipt = recovery::admit(
                &mut self.peer,
                self.recovery.as_deref_mut(),
                &mut self.receiver,
                &mut self.repair,
                cx,
                |receiver| job.complete(cx, receiver),
            )?;
            if let Some(f) = &mut self.feedback {
                f.complete(now(cx).map_err(Error::Session)?)
                    .map_err(Error::Feedback)?;
            }
            let Some(receipt) = receipt else {
                // Native borrowing ended, but this generation was fenced during
                // its network turn. No presentation, first-frame witness or input
                // authority may be manufactured from the obsolete completion.
                continue;
            };
            let stage = receipt.stage;
            let event = Presentation {
                frame: receipt.frame,
                stage,
            };
            if let Some(recovery) = &mut self.recovery {
                recovery
                    .observe_decoded(&receipt.decoded)
                    .map_err(Error::Recovery)?;
            }
            self.peer.decoded(receipt)?;
            self.statistics.presented(stage);
            // Completion releases the original decode reservation only after native borrowing.
            acquisition::notify(
                &mut self.peer,
                &self.receiver,
                &mut self.control,
                Some(event),
                ui,
            )?;
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
            // Repair is useful until the ORIGINAL missing reference expires.
            // An unrelated 80 ms send timeout killed otherwise recoverable
            // desktops while the admitted reference still had time to arrive.
            // Keep this exact offer through backpressure; neither a poll nor
            // transport admission creates a new lifetime or another attempt.
            self.until = offer.reference_deadline_us;
        }
        Ok(())
    }
}
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn network(
    peer: &mut Peer,
    clipboard: Option<&mut crate::native_clipboard::Application>,
    receiver: &mut ReceivePipeline,
    repair: &mut Repair,
    mut recovery: Option<&mut recovery_control::Receiver>,
    statistics: &mut Statistics,
    mut feedback: Option<&mut ViewerFeedback>,
    presentation: Option<&mut ViewerPresentation>,
    mut cursor: Option<&mut cursor::ViewerCursor>,
    mut audio: Option<&mut audio::ViewerAudio>,
    cx: &Cx,
    result: &mut impl FnMut(ResultEvent),
    other: &mut impl FnMut(Route, &[u8]) -> Result<Disposition, ()>,
) -> Result<(), Error> {
    if let Some(app) = clipboard
        && let Some(viewer) = peer.controlled()
    {
        app.viewer(viewer).map_err(Error::Clipboard)?;
    }
    recovery::prepare(peer, recovery.as_deref_mut(), receiver, repair, cx)?;
    let (session, media) = peer.parts()?;
    let routes = media
        .viewer_routes(&session.transport)
        .map_err(Error::Routes)?;
    if let Some(cursor) = cursor.as_deref_mut() {
        cursor.refresh(media, &session.transport)?;
    }
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
    if let Some(audio) = audio.as_deref_mut() {
        // Bounded local output progress and its one acknowledgement/stop.
        let current = now(cx).map_err(Error::Session)?;
        audio.service(&mut session.transport, cx, current, &mut || {
            cx.checkpoint().is_ok()
        })?;
    }
    let allow_recovery = recovery.is_some() && matches!(peer, Peer::Observe { .. });
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
            } else if let Some(cursor) = cursor.as_deref_mut().filter(|c| c.owns(route, bytes)) {
                // Before the media pipeline: cursor records are never progress,
                // fragments or freshness evidence.
                cursor.receive(route, bytes).map_err(|e| {
                    failure = Some(Error::Cursor(e));
                })?;
                Ok(Disposition::Consumed)
            } else if let Some(audio) = audio.as_deref_mut().filter(|a| a.owns(route, bytes)) {
                // Audio is never media progress, decode or freshness evidence;
                // its datagrams are always consumed (dropped when not active).
                audio.receive(route, bytes, &mut || cx.checkpoint().is_ok());
                Ok(Disposition::Consumed)
            } else if let Some((_, channel)) = routes.iter().find(|(r, _)| *r == route) {
                let current = now(cx).map_err(|e| {
                    failure = Some(Error::Session(e));
                })?;
                recovery::receive(receiver, *channel, bytes, current, allow_recovery).map_err(
                    |error| {
                        failure = Some(Error::Delivery(error));
                    },
                )?;
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
    let recovering = recovery::service(peer, recovery, receiver, repair, cx)?;
    // Source evidence is serviced independently of codec progress and before
    // advisory telemetry. No report can be derived from decode completion alone.
    if let Some(presentation) = presentation {
        let sample = if recovering {
            // A failure report cannot turn old pixels into a fresh view.
            ViewSample::Pending
        } else {
            peer.presented_sample(receiver)?
        };
        let session = peer.parent()?;
        presentation
            .service(
                &mut session.transport,
                cx,
                sample,
                now(cx).map_err(Error::Session)?,
            )
            .map_err(Error::PresentedState)?;
    }
    // The session's renewal, repair and input services get their turn first.
    if let Some(feedback) = feedback {
        let session = peer.parent()?;
        feedback
            .service(&mut session.transport, cx, now(cx).map_err(Error::Session)?)
            .map_err(Error::Feedback)?;
    }
    statistics.network_turns = statistics.network_turns.saturating_add(1);
    Ok(())
}

#[cfg(test)]
mod tests;
