//! Same-session recovery: old media is fenced, but parent renewal never stops.
//! One bounded configuration record and the ORIGINAL native decoder/receiver
//! cross the attachment and acknowledgement handoffs. No controller is promoted:
//! a control grant stays suspended until the recovered view's evidence and a
//! new ticket resume it (plan 11.3, 12.3).
use super::{
    Error, Presentation, Presenter, ViewerSession, acquisition, decoder_startup, feedback, now,
    recovery_control,
};
use crate::{
    media::PresentationReceipt, media_quic::NegotiatedMedia, session_startup::ControlledViewer,
};
use asupersync::cx::Cx;
use fr_client::input::ResultEvent;
use fr_media::delivery::ReceivePipeline;
use fr_transport::quic::{Disposition, Route, StreamRoute};
use fr_wire::{
    Kind,
    attachment::{self, MediaRole, Message},
    decoder::Binding,
    input::{InputDelivery, InputDirection},
    receiver_metrics,
};
use std::{cell::Cell, future::Future, pin::pin, task::Poll, time::Duration};

/// The parent a recovery keeps serving while old media is fenced: an
/// observation session, or a control grant whose suspended input keeps its
/// renewals, tickets and results serviced through its own drive.
enum Parent<'a> {
    Observe(&'a mut ViewerSession),
    Control(&'a mut ControlledViewer, &'a mut dyn FnMut(ResultEvent)),
}
/// The original parent and audio output stay together for EVERY recovery
/// network turn. Receiving packets alone cannot make native playback progress.
/// No audio generation, queue, deadline or decoder is created by video recovery.
pub(super) struct Link<'a> {
    parent: Parent<'a>,
    audio: Option<&'a mut super::audio::ViewerAudio>,
    refused: &'a Cell<Option<Error>>,
}
impl Link<'_> {
    fn session(&mut self) -> &mut ViewerSession {
        match &mut self.parent {
            Parent::Observe(session) => session,
            Parent::Control(viewer, _) => viewer.session_mut(),
        }
    }
    async fn drive(
        &mut self,
        wait: Duration,
        mut dispatch: impl FnMut(Route, &[u8]) -> Result<Disposition, ()>,
    ) -> Result<(), Error> {
        let Self {
            parent,
            audio,
            refused,
        } = self;
        let session = match parent {
            Parent::Observe(session) => &mut **session,
            Parent::Control(viewer, _) => viewer.session_mut(),
        };
        // Playback depends on the original observation, not the fenced video
        // receiver or a suspended input grant. Check before native service and
        // again at its bounded callbacks; a quiet network still gets a turn.
        session.check().map_err(Error::Session)?;
        let cx = session.cx.clone();
        let heard_until = session.heard_until;
        let mut live = || {
            cx.checkpoint().is_ok() && now(&cx).is_ok_and(|current| current < heard_until)
        };
        if let Some(audio) = audio.as_deref_mut() {
            audio.service(
                &mut session.transport,
                &cx,
                now(&cx).map_err(Error::Session)?,
                &mut live,
            )?;
        }
        // Preserve exactly one audio owner across local progress and dispatch.
        // Its pending configuration/stop reply retains its original deadline.
        // Parent drive validates its renewed observation before dispatch. Do
        // not hold the pre-drive heard deadline over a newly accepted renewal.
        let routed = lanes(
            audio.as_deref_mut(),
            || cx.checkpoint().is_ok(),
            refused,
            &mut dispatch,
        );
        match parent {
            Parent::Observe(session) => session.drive(wait, routed).await.map_err(Error::Session),
            Parent::Control(viewer, result) => viewer
                .drive(wait, &mut **result, routed)
                .await
                .map_err(Error::Control),
        }
    }
    fn close(&mut self) {
        match &mut self.parent {
            Parent::Observe(session) => session.close(),
            Parent::Control(viewer, _) => viewer.close(),
        }
    }
    /// An interactive embedding sees its grant, not an observer.
    fn notify(
        &mut self,
        budget: &Budget,
        ui: &mut impl FnMut(acquisition::Dispatch<'_>, Option<Presentation>) -> Result<(), ()>,
    ) -> Result<(), Error> {
        budget.remaining()?;
        let state = match &mut self.parent {
            Parent::Observe(_) => acquisition::State::Observing,
            Parent::Control(viewer, _) => acquisition::State::Controlled(viewer),
        };
        ui(acquisition::Dispatch::Existing(state), None).map_err(|()| Error::Application)?;
        budget.remaining()?;
        Ok(())
    }
}

/// This guard exists before polling, including before the first native await.
/// Failure/cancellation closes the original parent BEFORE decoder cleanup.
struct Attempt<'a> {
    link: Link<'a>,
    presenter: &'a mut Presenter,
    receiver: &'a mut ReceivePipeline,
    completed: bool,
}
impl Drop for Attempt<'_> {
    fn drop(&mut self) {
        if !self.completed {
            self.link.close();
            self.receiver.close();
            self.presenter.abort();
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn resume<'a>(
    link: Link<'a>,
    previous: NegotiatedMedia,
    report: &'a mut recovery_control::Receiver,
    presenter: &'a mut Presenter,
    receiver: &'a mut ReceivePipeline,
    ui: &'a mut impl FnMut(acquisition::Dispatch<'_>, Option<Presentation>) -> Result<(), ()>,
    other: &'a mut impl FnMut(Route, &[u8]) -> Result<Disposition, ()>,
    refused: &'a Cell<Option<Error>>,
) -> impl Future<Output = Result<(NegotiatedMedia, PresentationReceipt), Error>> + 'a {
    let attempt = Attempt {
        link,
        presenter,
        receiver,
        completed: false,
    };
    async move {
        let mut attempt = attempt;
        let result = Box::pin(run(
            &mut attempt.link,
            previous,
            report,
            attempt.presenter,
            attempt.receiver,
            ui,
            other,
            refused,
        ))
        .await?;
        attempt.completed = true;
        Ok(result)
    }
}

struct Budget {
    cx: Cx,
    until: u64,
}
impl Budget {
    fn remaining(&self) -> Result<Duration, Error> {
        self.cx.checkpoint().map_err(|_| Error::Closed)?;
        self.until
            .checked_sub(now(&self.cx).map_err(Error::Session)?)
            .filter(|&n| n > 0)
            .map(Duration::from_micros)
            .ok_or(Error::Startup(decoder_startup::Error::Expired))
    }
    fn turn(&self) -> Result<Duration, Error> {
        Ok(self.remaining()?.min(Duration::from_millis(5)))
    }
    fn permitted(&self, heard_until: u64) -> bool {
        self.cx.checkpoint().is_ok()
            && now(&self.cx).is_ok_and(|n| n < self.until && n < heard_until)
    }
}

/// Old advisory queries are validated and retired, not answered using a new
/// receiver's counters. Their requester expires them to unknown. New-generation
/// queries stay queued for the new feedback owner after the handshake finishes.
struct Dispatch<'a> {
    inbound: Route,
    old: Binding,
    next: Binding,
    metrics: bool,
    limits: fr_core::limits::ProtocolLimits,
    obsolete: [Route; 4],
    /// The new generation's media lanes and its configuration stream, which
    /// after the decoder configuration carries only cursor shapes: held for
    /// their owners once the recovery completes.
    active: Option<[Route; 4]>,
    /// Why a callback refused, so the drive's bare handler refusal is named.
    refused: &'a Cell<Option<Error>>,
}
impl Dispatch<'_> {
    fn receive(
        &self,
        route: Route,
        bytes: &[u8],
        other: &mut impl FnMut(Route, &[u8]) -> Result<Disposition, ()>,
    ) -> Result<Disposition, ()> {
        if self.obsolete.contains(&route) {
            return Ok(Disposition::Consumed);
        }
        if feedback::is_feedback(bytes) {
            if !self.metrics || route != self.inbound {
                return self.refuse(Error::Feedback(feedback::Error::Binding));
            }
            let decode = |binding| {
                receiver_metrics::decode(
                    bytes,
                    binding,
                    &self.limits,
                    InputDirection::HostToViewer,
                    InputDelivery::Reliable,
                )
            };
            if matches!(
                decode(self.old),
                Ok(receiver_metrics::Message::Query { .. })
            ) {
                return Ok(Disposition::Consumed);
            }
            if matches!(
                decode(self.next),
                Ok(receiver_metrics::Message::Query { .. })
            ) {
                return Ok(Disposition::Blocked);
            }
            return self.refuse(Error::Feedback(feedback::Error::Binding));
        }
        if self.active.is_some_and(|routes| routes.contains(&route)) {
            return Ok(Disposition::Blocked);
        }
        // These are consumed by the original role-specific exchange, never an
        // embedding application's callback. Parent renewal/clock dispatch runs
        // before this callback in ViewerSession::drive.
        if route == self.inbound
            && bytes.get(6..8).is_some_and(|kind| {
                (0x0018..=0x001c).contains(&u16::from_be_bytes([kind[0], kind[1]]))
            })
        {
            return Ok(Disposition::Blocked);
        }
        other(route, bytes)
    }
    /// The first refusal of a drive ends it; keep that one.
    fn refuse(&self, error: Error) -> Result<Disposition, ()> {
        if self.refused.get().is_none() {
            self.refused.set(Some(error));
        }
        Err(())
    }
}
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn run(
    link: &mut Link<'_>,
    previous: NegotiatedMedia,
    report: &mut recovery_control::Receiver,
    presenter: &mut Presenter,
    receiver: &mut ReceivePipeline,
    ui: &mut impl FnMut(acquisition::Dispatch<'_>, Option<Presentation>) -> Result<(), ()>,
    other: &mut impl FnMut(Route, &[u8]) -> Result<Disposition, ()>,
    refused: &Cell<Option<Error>>,
) -> Result<(NegotiatedMedia, PresentationReceipt), Error> {
    let session = link.session();
    session.check().map_err(Error::Session)?;
    if report.state() != recovery_control::State::Requested {
        return Err(Error::Closed);
    }
    let budget = Budget {
        cx: session.cx.clone(),
        until: report.next_deadline().ok_or(Error::Closed)?,
    };
    let mut old = previous.binding();
    old.parent = session.opened.binding;
    let mut next = old;
    next.recovery = next.recovery.next().ok_or(Error::Closed)?;
    let mut dispatch = Dispatch {
        inbound: Route::Stream(session.routes.inbound),
        old,
        next,
        metrics: session.opened.selection.capabilities.iter().any(|c| {
            c.name == receiver_metrics::CAPABILITY && c.version == receiver_metrics::VERSION
        }),
        limits: session.opened.selection.limits,
        active: None,
        obsolete: previous
            .retiring_viewer_routes(&session.transport)
            .map_err(Error::Routes)?,
        refused,
    };
    // A locally queued request is not host receipt. Wait for the authenticated
    // NEXT configuration offer before sending resets for our old media streams.
    // The offer stays in its original bounded receive slot for Replacement.
    loop {
        let session = link.session();
        let heard = session.heard_until;
        report
            .service(&budget.cx, &mut session.transport, receiver, || {
                budget.permitted(heard)
            })
            .map_err(Error::Recovery)?;
        if replacement_offered(session, &budget, &dispatch)? {
            break;
        }
        link.drive(budget.turn()?, |r, b| dispatch.receive(r, b, other))
            .await?;
        link.notify(&budget, ui)?;
    }
    let session = link.session();
    let heard = session.heard_until;
    let mut replacement = previous
        .begin_replacement(
            &budget.cx,
            &mut session.transport,
            session.routes,
            session.opened.binding,
            budget.until,
            None,
            || budget.permitted(heard),
        )
        .map_err(Error::Replacement)?;
    loop {
        let session = link.session();
        let heard = session.heard_until;
        if replacement
            .advance(&budget.cx, &mut session.transport, || {
                budget.permitted(heard)
            })
            .map_err(Error::Replacement)?
        {
            break;
        }
        link.drive(budget.turn()?, |r, b| {
            if replacement.owns_record(r, b) {
                Ok(Disposition::Blocked)
            } else {
                dispatch.receive(r, b, other)
            }
        })
        .await?;
        link.notify(&budget, ui)?;
    }
    let session = link.session();
    let media = replacement
        .finish(&budget.cx, &mut session.transport)
        .map_err(Error::Replacement)?;
    let configuration = media
        .viewer_configuration_route(&session.transport)
        .map_err(Error::Routes)?;
    let [(recovery, _), (video, _), (datagram, _)] = media
        .viewer_routes(&session.transport)
        .map_err(Error::Routes)?;
    dispatch.active = Some([recovery, video, datagram, Route::Stream(configuration)]);
    let config = configuration_record(link, &budget, configuration, &dispatch, ui, other).await?;
    let mut handshake = decoder_startup::ViewerRecovery::prepare(
        budget.cx.clone(),
        &link.session().transport,
        &media,
        &config,
        budget.until,
        report,
        presenter,
        receiver,
    )
    .map_err(Error::Startup)?;
    drop(config);
    // Do not poll the old requestor after replacement: its failed scope is
    // deliberately invalid. The borrowed handshake retains that same deadline.
    send_ack(link, &budget, &mut handshake, &dispatch, ui, other).await?;
    let receipt = loop {
        let session = link.session();
        let heard = session.heard_until;
        let mut failure = None;
        media
            .receive_ready(
                &budget.cx,
                &mut session.transport,
                || budget.permitted(heard),
                |channel, bytes| {
                    handshake.receive_media(channel, bytes).map_err(|e| {
                        failure = Some(e);
                    })?;
                    Ok(Disposition::Consumed)
                },
            )
            .map_err(|e| failure.map_or(Error::Routes(e), Error::Startup))?;
        let decoded = during(
            link,
            &budget,
            handshake.present_first(),
            &dispatch,
            ui,
            other,
        )
        .await?;
        handshake
            .tick(&link.session().transport)
            .map_err(Error::Startup)?;
        if let Some(receipt) = decoded {
            break receipt;
        }
    };
    send_ack(link, &budget, &mut handshake, &dispatch, ui, other).await?;
    let session = link.session();
    handshake
        .finish(&session.transport, &media)
        .map_err(Error::Startup)?;
    session.check().map_err(Error::Session)?;
    budget.remaining()?;
    Ok((media, receipt))
}

fn replacement_offered(
    session: &mut ViewerSession,
    budget: &Budget,
    dispatch: &Dispatch<'_>,
) -> Result<bool, Error> {
    let offered = Cell::new(false);
    let mut failure = None;
    let heard = session.heard_until;
    session
        .transport
        .receive_ready(
            &budget.cx,
            || budget.permitted(heard),
            |r| r == dispatch.inbound,
            |_, bytes| {
                if bytes.get(6..8) != Some(&(Kind::StreamBinding as u16).to_be_bytes()) {
                    return Ok(Disposition::Blocked);
                }
                let parsed = attachment::decode(
                    bytes,
                    dispatch.old.parent,
                    dispatch.old.parent.id,
                    &dispatch.limits,
                    InputDirection::HostToViewer,
                    InputDelivery::Reliable,
                );
                let valid = match parsed {
                    Ok(Message::Binding(d)) => {
                        let mut view = d.binding;
                        view.parent = dispatch.old.parent;
                        view == dispatch.next && d.role == MediaRole::Configuration
                    }
                    _ => false,
                };
                if !valid {
                    failure = Some(Error::Replacement(
                        crate::media_quic::replacement::Error::WrongBinding,
                    ));
                    return Err(());
                }
                offered.set(true);
                Ok(Disposition::Blocked)
            },
        )
        .map_err(|e| failure.unwrap_or(Error::Transport(e)))?;
    Ok(offered.get())
}

async fn configuration_record(
    link: &mut Link<'_>,
    budget: &Budget,
    route: StreamRoute,
    dispatch: &Dispatch<'_>,
    ui: &mut impl FnMut(acquisition::Dispatch<'_>, Option<Presentation>) -> Result<(), ()>,
    other: &mut impl FnMut(Route, &[u8]) -> Result<Disposition, ()>,
) -> Result<Vec<u8>, Error> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(route.maximum)
        .map_err(|_| Error::Startup(decoder_startup::Error::Allocation))?;
    if bytes.capacity() > dispatch.limits.max_control_message_bytes() as usize {
        return Err(Error::Startup(decoder_startup::Error::Allocation));
    }
    loop {
        budget.remaining()?;
        let available = Cell::new(true);
        let session = link.session();
        let heard = session.heard_until;
        session
            .transport
            .receive_ready(
                &budget.cx,
                || budget.permitted(heard),
                |r| available.get() && r == Route::Stream(route),
                |_, record| {
                    if record.len() > route.maximum {
                        return Err(());
                    }
                    bytes.extend_from_slice(record);
                    available.set(false);
                    Ok(Disposition::Consumed)
                },
            )
            .map_err(Error::Transport)?;
        if !bytes.is_empty() {
            return Ok(bytes);
        }
        link.drive(budget.turn()?, |r, b| {
            if r == Route::Stream(route) {
                Ok(Disposition::Blocked)
            } else {
                dispatch.receive(r, b, other)
            }
        })
        .await?;
        link.notify(budget, ui)?;
    }
}

async fn send_ack(
    link: &mut Link<'_>,
    budget: &Budget,
    handshake: &mut decoder_startup::ViewerRecovery<'_>,
    dispatch: &Dispatch<'_>,
    ui: &mut impl FnMut(acquisition::Dispatch<'_>, Option<Presentation>) -> Result<(), ()>,
    other: &mut impl FnMut(Route, &[u8]) -> Result<Disposition, ()>,
) -> Result<(), Error> {
    loop {
        budget.remaining()?;
        let session = link.session();
        session.check().map_err(Error::Session)?;
        let sent = handshake
            .transmit(&mut session.transport)
            .map_err(Error::Startup)?;
        link.drive(budget.turn()?, |r, b| dispatch.receive(r, b, other))
            .await?;
        handshake
            .tick(&link.session().transport)
            .map_err(Error::Startup)?;
        link.notify(budget, ui)?;
        if sent {
            return Ok(());
        }
    }
}

/// Finish an already-polled parent network turn even when native success wins.
/// A decoder wait never borrows QUIC and never stops observation challenge service.
async fn during<T>(
    link: &mut Link<'_>,
    budget: &Budget,
    native: impl Future<Output = Result<T, decoder_startup::Error>>,
    dispatch: &Dispatch<'_>,
    ui: &mut impl FnMut(acquisition::Dispatch<'_>, Option<Presentation>) -> Result<(), ()>,
    other: &mut impl FnMut(Route, &[u8]) -> Result<Disposition, ()>,
) -> Result<T, Error> {
    let mut native = pin!(native);
    loop {
        let mut result = None;
        {
            let mut turn = pin!(link.drive(budget.turn()?, |r, b| dispatch.receive(r, b, other)));
            std::future::poll_fn(|task| {
                budget.remaining()?;
                if result.is_none()
                    && let Poll::Ready(value) = native.as_mut().poll(task)
                {
                    result = Some(value);
                }
                if let Some(Err(e)) = &result {
                    return Poll::Ready(Err(Error::Startup(*e)));
                }
                turn.as_mut().poll(task)
            })
            .await?;
        }
        link.notify(budget, ui)?;
        if let Some(value) = result {
            return value.map_err(Error::Startup);
        }
    }
}

/// Audio is not video: its lanes outlive the replaced media, so it keeps
/// playing through a recovery. A record no lane claims is named.
pub(super) fn lanes<'a>(
    mut audio: Option<&'a mut super::audio::ViewerAudio>,
    mut live: impl FnMut() -> bool + 'a,
    refused: &'a Cell<Option<Error>>,
    other: &'a mut impl FnMut(Route, &[u8]) -> Result<Disposition, ()>,
) -> impl FnMut(Route, &[u8]) -> Result<Disposition, ()> + 'a {
    move |route, bytes| {
        if let Some(audio) = audio.as_deref_mut().filter(|a| a.owns(route, bytes)) {
            audio.receive(route, bytes, &mut live);
            return Ok(Disposition::Consumed);
        }
        other(route, bytes).map_err(|()| {
            if refused.get().is_none() {
                refused.set(Some(super::unclaimed(bytes)));
            }
        })
    }
}

impl super::StreamingViewer {
    /// Resume this stream on its recovered generation, on the same session:
    /// an observer's, or a control grant's (suspended until resumed by the
    /// recovered view's evidence and a new ticket).
    pub(super) async fn resume_stream(
        &mut self,
        ui: &mut impl FnMut(acquisition::Dispatch<'_>, Option<Presentation>) -> Result<(), ()>,
        result: &mut impl FnMut(ResultEvent),
        other: &mut impl FnMut(Route, &[u8]) -> Result<Disposition, ()>,
    ) -> Result<Presentation, Error> {
        self.initial = None;
        self.repair.clear();
        // Only lifetime counters cross epochs; old pending replies and source
        // evidence do not. Late old metrics are explicitly retired by Dispatch.
        let feedback_sent = self.feedback.take().map_or(0, |f| f.sent);
        let presentation_sent = self.presentation.take().map_or(0, |p| p.sent);
        let refused = Cell::new(None);
        // A drive ends on its callback's first refusal: report that, not the
        // transport's bare Handler error.
        let named = |error| refused.take().unwrap_or(error);
        match std::mem::replace(&mut self.peer, super::Peer::Closed) {
            super::Peer::Observe { mut session, media } => {
                let (media, receipt) = resume(
                    Link {
                        parent: Parent::Observe(&mut session),
                        audio: self.audio.as_mut(),
                        refused: &refused,
                    },
                    media,
                    self.recovery.as_mut().ok_or(Error::Closed)?,
                    &mut self.presenter,
                    &mut self.receiver,
                    ui,
                    other,
                    &refused,
                )
                .await
                .map_err(named)?;
                let presentation = self.reinstall(
                    &mut session,
                    &media,
                    &receipt,
                    feedback_sent,
                    presentation_sent,
                )?;
                self.peer = super::Peer::Observe { session, media };
                Ok(presentation)
            }
            super::Peer::Control(mut viewer) => {
                let previous = match viewer.take_media_for_recovery() {
                    Ok(previous) => previous,
                    Err(error) => {
                        viewer.close();
                        return Err(Error::Control(error));
                    }
                };
                let (media, receipt) = resume(
                    Link {
                        parent: Parent::Control(&mut viewer, result),
                        audio: self.audio.as_mut(),
                        refused: &refused,
                    },
                    previous,
                    self.recovery.as_mut().ok_or(Error::Closed)?,
                    &mut self.presenter,
                    &mut self.receiver,
                    ui,
                    other,
                    &refused,
                )
                .await
                .map_err(named)?;
                let presentation = self.reinstall(
                    viewer.session_mut(),
                    &media,
                    &receipt,
                    feedback_sent,
                    presentation_sent,
                )?;
                if let Err(error) = viewer.install_recovered_media(media, &self.receiver) {
                    viewer.close();
                    return Err(Error::Control(error));
                }
                // follow_recovery installed an EMPTY presentation tracker. The
                // handshake consumed the first recovered IDR outside serve's
                // normal decode loop, so move its actual completion into that
                // tracker BEFORE exposing the frame to a visibility callback.
                // This does not call visible(), renew a ticket, or reset either
                // the original display deadline or the source observation.
                let submitted =
                    receipt.stage == crate::media::PresentationStage::SubmittedToCompositor;
                if let Err(error) = viewer.decoded(receipt.decoded, submitted) {
                    viewer.close();
                    return Err(Error::Control(error));
                }
                self.peer = super::Peer::Control(viewer);
                Ok(presentation)
            }
            other_peer => {
                self.peer = other_peer;
                Err(Error::Closed)
            }
        }
    }
    /// New-generation report, feedback and presentation owners: only lifetime
    /// counters cross the recovery, never old evidence.
    fn reinstall(
        &mut self,
        session: &mut ViewerSession,
        media: &NegotiatedMedia,
        receipt: &PresentationReceipt,
        feedback_sent: u64,
        presentation_sent: u64,
    ) -> Result<Presentation, Error> {
        let mut report = media
            .recovery_receiver(
                &session.transport,
                session.routes,
                session.opened.binding,
                &self.receiver,
            )
            .map_err(Error::Recovery)?;
        report
            .observe_decoded(&receipt.decoded)
            .map_err(Error::Recovery)?;
        let setup = feedback::Setup::selected(
            &session.opened.selection,
            session.opened.binding,
            media.binding(),
        )
        .map_err(Error::Feedback)?;
        self.feedback = setup
            .map(|s| {
                feedback::ViewerFeedback::new(
                    s,
                    Route::Stream(session.routes.inbound),
                    Route::Stream(session.routes.outbound),
                )
            })
            .transpose()
            .map_err(Error::Feedback)?;
        if let Some(feedback) = &mut self.feedback {
            feedback.sent = feedback_sent;
        }
        self.presentation = super::ViewerPresentation::attach(
            &session.opened.selection,
            session.opened.binding,
            media.binding(),
            &session.transport,
            session.routes.outbound,
            now(&session.cx).map_err(Error::Session)?,
        )
        .map_err(Error::PresentedState)?;
        if let Some(presentation) = &mut self.presentation {
            presentation.sent = presentation_sent;
        }
        self.presenter
            .check_stream(&session.transport, media, &self.receiver)
            .map_err(Error::Media)?;
        self.recovery = Some(Box::new(report));
        self.statistics.presented(receipt.stage);
        self.statistics.recovered_streams = self.statistics.recovered_streams.saturating_add(1);
        Ok(Presentation {
            frame: receipt.frame,
            stage: receipt.stage,
        })
    }
}

#[cfg(test)]
mod audio_tests;
