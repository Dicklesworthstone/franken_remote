//! Same-session recovery: old media is fenced, but parent renewal never stops.
//! One bounded configuration record and the ORIGINAL native decoder/receiver
//! cross the attachment and acknowledgement handoffs. No controller is promoted.
use super::{
    Error, Presentation, Presenter, ViewerSession, acquisition, decoder_startup, feedback, now,
    recovery_control,
};
use crate::{media::PresentationReceipt, media_quic::NegotiatedMedia};
use asupersync::cx::Cx;
use fr_media::delivery::ReceivePipeline;
use fr_transport::quic::{Disposition, Route, StreamRoute};
use fr_wire::{
    Kind,
    attachment::{self, MediaRole, Message},
    decoder::Binding,
    input::{InputDelivery, InputDirection},
    negotiation::Role,
    receiver_metrics,
};
use std::{cell::Cell, future::Future, pin::pin, task::Poll, time::Duration};

/// This guard exists before polling, including before the first native await.
/// Failure/cancellation closes the original parent BEFORE decoder cleanup.
struct Attempt<'a> {
    session: &'a mut ViewerSession,
    presenter: &'a mut Presenter,
    receiver: &'a mut ReceivePipeline,
    completed: bool,
}
impl Drop for Attempt<'_> {
    fn drop(&mut self) {
        if !self.completed {
            self.session.close();
            self.receiver.close();
            self.presenter.abort();
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn resume<'a>(
    session: &'a mut ViewerSession,
    previous: NegotiatedMedia,
    report: &'a mut recovery_control::Receiver,
    presenter: &'a mut Presenter,
    receiver: &'a mut ReceivePipeline,
    ui: &'a mut impl FnMut(acquisition::Dispatch<'_>, Option<Presentation>) -> Result<(), ()>,
    other: &'a mut impl FnMut(Route, &[u8]) -> Result<Disposition, ()>,
) -> impl Future<Output = Result<(NegotiatedMedia, PresentationReceipt), Error>> + 'a {
    let attempt = Attempt {
        session,
        presenter,
        receiver,
        completed: false,
    };
    async move {
        let mut attempt = attempt;
        let result = Box::pin(run(
            attempt.session,
            previous,
            report,
            attempt.presenter,
            attempt.receiver,
            ui,
            other,
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
struct Dispatch {
    inbound: Route,
    old: Binding,
    next: Binding,
    metrics: bool,
    limits: fr_core::limits::ProtocolLimits,
    obsolete: [Route; 4],
    active: Option<[Route; 3]>,
}
impl Dispatch {
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
                return Err(());
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
            return Err(());
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
}
fn notify(
    budget: &Budget,
    ui: &mut impl FnMut(acquisition::Dispatch<'_>, Option<Presentation>) -> Result<(), ()>,
) -> Result<(), Error> {
    budget.remaining()?;
    ui(
        acquisition::Dispatch::Existing(acquisition::State::Observing),
        None,
    )
    .map_err(|()| Error::Application)?;
    budget.remaining()?;
    Ok(())
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn run(
    session: &mut ViewerSession,
    previous: NegotiatedMedia,
    report: &mut recovery_control::Receiver,
    presenter: &mut Presenter,
    receiver: &mut ReceivePipeline,
    ui: &mut impl FnMut(acquisition::Dispatch<'_>, Option<Presentation>) -> Result<(), ()>,
    other: &mut impl FnMut(Route, &[u8]) -> Result<Disposition, ()>,
) -> Result<(NegotiatedMedia, PresentationReceipt), Error> {
    session.check().map_err(Error::Session)?;
    if session.opened.selection.role != Role::Observe
        || report.state() != recovery_control::State::Requested
    {
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
    };
    // A locally queued request is not host receipt. Wait for the authenticated
    // NEXT configuration offer before sending resets for our old media streams.
    // The offer stays in its original bounded receive slot for Replacement.
    loop {
        let heard = session.heard_until;
        report
            .service(&budget.cx, &mut session.transport, receiver, || {
                budget.permitted(heard)
            })
            .map_err(Error::Recovery)?;
        if replacement_offered(session, &budget, &dispatch)? {
            break;
        }
        session
            .drive(budget.turn()?, |r, b| dispatch.receive(r, b, other))
            .await
            .map_err(Error::Session)?;
        notify(&budget, ui)?;
    }
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
        let heard = session.heard_until;
        if replacement
            .advance(&budget.cx, &mut session.transport, || {
                budget.permitted(heard)
            })
            .map_err(Error::Replacement)?
        {
            break;
        }
        session
            .drive(budget.turn()?, |r, b| {
                if replacement.owns_record(r, b) {
                    Ok(Disposition::Blocked)
                } else {
                    dispatch.receive(r, b, other)
                }
            })
            .await
            .map_err(Error::Session)?;
        notify(&budget, ui)?;
    }
    let media = replacement
        .finish(&budget.cx, &mut session.transport)
        .map_err(Error::Replacement)?;
    dispatch.active = Some(
        media
            .viewer_routes(&session.transport)
            .map_err(Error::Routes)?
            .map(|(r, _)| r),
    );
    let configuration = media
        .viewer_configuration_route(&session.transport)
        .map_err(Error::Routes)?;
    let config =
        configuration_record(session, &budget, configuration, &dispatch, ui, other).await?;
    let mut handshake = decoder_startup::ViewerRecovery::prepare(
        budget.cx.clone(),
        &session.transport,
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
    send_ack(session, &budget, &mut handshake, &dispatch, ui, other).await?;
    let receipt = loop {
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
            session,
            &budget,
            handshake.present_first(),
            &dispatch,
            ui,
            other,
        )
        .await?;
        handshake.tick(&session.transport).map_err(Error::Startup)?;
        if let Some(receipt) = decoded {
            break receipt;
        }
    };
    send_ack(session, &budget, &mut handshake, &dispatch, ui, other).await?;
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
    dispatch: &Dispatch,
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
    session: &mut ViewerSession,
    budget: &Budget,
    route: StreamRoute,
    dispatch: &Dispatch,
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
        session
            .drive(budget.turn()?, |r, b| {
                if r == Route::Stream(route) {
                    Ok(Disposition::Blocked)
                } else {
                    dispatch.receive(r, b, other)
                }
            })
            .await
            .map_err(Error::Session)?;
        notify(budget, ui)?;
    }
}

async fn send_ack(
    session: &mut ViewerSession,
    budget: &Budget,
    handshake: &mut decoder_startup::ViewerRecovery<'_>,
    dispatch: &Dispatch,
    ui: &mut impl FnMut(acquisition::Dispatch<'_>, Option<Presentation>) -> Result<(), ()>,
    other: &mut impl FnMut(Route, &[u8]) -> Result<Disposition, ()>,
) -> Result<(), Error> {
    loop {
        budget.remaining()?;
        session.check().map_err(Error::Session)?;
        let sent = handshake
            .transmit(&mut session.transport)
            .map_err(Error::Startup)?;
        session
            .drive(budget.turn()?, |r, b| dispatch.receive(r, b, other))
            .await
            .map_err(Error::Session)?;
        handshake.tick(&session.transport).map_err(Error::Startup)?;
        notify(budget, ui)?;
        if sent {
            return Ok(());
        }
    }
}

/// Finish an already-polled parent network turn even when native success wins.
/// A decoder wait never borrows QUIC and never stops observation challenge service.
async fn during<T>(
    session: &mut ViewerSession,
    budget: &Budget,
    native: impl Future<Output = Result<T, decoder_startup::Error>>,
    dispatch: &Dispatch,
    ui: &mut impl FnMut(acquisition::Dispatch<'_>, Option<Presentation>) -> Result<(), ()>,
    other: &mut impl FnMut(Route, &[u8]) -> Result<Disposition, ()>,
) -> Result<T, Error> {
    let mut native = pin!(native);
    loop {
        let mut result = None;
        {
            let mut turn =
                pin!(session.drive(budget.turn()?, |r, b| dispatch.receive(r, b, other)));
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
                turn.as_mut().poll(task).map_err(Error::Session)
            })
            .await?;
        }
        notify(budget, ui)?;
        if let Some(value) = result {
            return value.map_err(Error::Startup);
        }
    }
}

impl super::StreamingViewer {
    pub(super) async fn resume_observation(
        &mut self,
        ui: &mut impl FnMut(acquisition::Dispatch<'_>, Option<Presentation>) -> Result<(), ()>,
        other: &mut impl FnMut(Route, &[u8]) -> Result<Disposition, ()>,
    ) -> Result<Presentation, Error> {
        let super::Peer::Observe { mut session, media } =
            std::mem::replace(&mut self.peer, super::Peer::Closed)
        else {
            return Err(Error::Closed);
        };
        self.initial = None;
        self.repair.clear();
        // Only lifetime counters cross epochs; old pending replies and source
        // evidence do not. Late old metrics are explicitly retired by Dispatch.
        let feedback_sent = self.feedback.take().map_or(0, |f| f.sent);
        let presentation_sent = self.presentation.take().map_or(0, |p| p.sent);
        let (media, receipt) = resume(
            &mut session,
            media,
            self.recovery.as_mut().ok_or(Error::Closed)?,
            &mut self.presenter,
            &mut self.receiver,
            ui,
            other,
        )
        .await?;
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
            .check_stream(&session.transport, &media, &self.receiver)
            .map_err(Error::Media)?;
        self.recovery = Some(report);
        self.statistics.presented(receipt.stage);
        self.statistics.recovered_streams = self.statistics.recovered_streams.saturating_add(1);
        self.peer = super::Peer::Observe { session, media };
        Ok(Presentation {
            frame: receipt.frame,
            stage: receipt.stage,
        })
    }
}
