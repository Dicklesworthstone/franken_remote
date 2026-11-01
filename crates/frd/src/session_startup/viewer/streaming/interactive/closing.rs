//! Closing policy on the original interactive watch/request/grant service.
use super::{
    Cx, Dispatch, Error, Guarded, InteractiveState, NegotiatedInput, Operation, Peer, Policy,
    Presentation, Request, ResultEvent, State, StreamingViewer, acquisition,
};
use fr_transport::{
    ControlCloseOutcome,
    quic::{CloseOutcome, Disposition, Route},
};
use fr_wire::closure::Reason;
use std::{future::Future, ops::ControlFlow};

/// The exact original session's closing exchange. Viewing never fabricates a
/// lease or release report; Controlled preserves its actual granted lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InteractiveCloseOutcome {
    Viewing(CloseOutcome),
    Controlled(ControlCloseOutcome),
}

impl StreamingViewer {
    /// Watch, explicitly request control, and close the actually granted lease
    /// without restarting service or replacing the original media/receipt owners.
    /// Before a grant, Break cancels locally with Closed: no lease or controller
    /// report is fabricated. The same call-time guards also cover abandonment.
    ///
    /// After a grant, Break prepares the existing closing exchange INSIDE the
    /// UI turn, fencing input before the pending native decode can unwind. Media
    /// is then retired before polling terminal I/O. No further callbacks execute.
    /// Emergency stop, callback failure and protocol errors remain terminal errors.
    /// The independently provisioned cleanup context is not replacement authority;
    /// the original security checks and fixed closing deadlines remain unchanged.
    #[allow(clippy::too_many_arguments)]
    pub fn serve_interactive_control_until<'a>(
        &'a mut self,
        channels: NegotiatedInput,
        request: Request,
        policy: Policy,
        cleanup: &'a Cx,
        ui: impl FnMut(InteractiveState<'_>, Option<Presentation>) -> Result<ControlFlow<Reason>, ()>
        + 'a,
        result: impl FnMut(ResultEvent) + 'a,
        other: impl FnMut(Route, &[u8]) -> Result<Disposition, ()> + 'a,
    ) -> impl Future<Output = Result<ControlCloseOutcome, Error>> + 'a {
        let run = self.serve_interactive_closing(
            channels, request, policy, cleanup, false, ui, result, other,
        );
        async move {
            match run.await? {
                InteractiveCloseOutcome::Controlled(outcome) => Ok(outcome),
                InteractiveCloseOutcome::Viewing(_) => Err(Error::Closed),
            }
        }
    }

    /// Close this original session after an explicit local decision, either
    /// while it is still viewing with NO control request or after an actual
    /// grant. The former uses the ordinary session-close exchange and cannot
    /// invent a lease. Requesting, including a request staged by this same UI
    /// callback, remains terminal and does not become a successful viewing close.
    /// All media/native owners stay retained for the usual mandatory cleanup.
    #[allow(clippy::too_many_arguments)]
    pub fn serve_interactive_session_until<'a>(
        &'a mut self,
        channels: NegotiatedInput,
        request: Request,
        policy: Policy,
        cleanup: &'a Cx,
        ui: impl FnMut(InteractiveState<'_>, Option<Presentation>) -> Result<ControlFlow<Reason>, ()>
        + 'a,
        result: impl FnMut(ResultEvent) + 'a,
        other: impl FnMut(Route, &[u8]) -> Result<Disposition, ()> + 'a,
    ) -> impl Future<Output = Result<InteractiveCloseOutcome, Error>> + 'a {
        self.serve_interactive_closing(channels, request, policy, cleanup, true, ui, result, other)
    }

    #[allow(clippy::too_many_arguments)]
    fn serve_interactive_closing<'a>(
        &'a mut self,
        channels: NegotiatedInput,
        request: Request,
        policy: Policy,
        cleanup: &'a Cx,
        close_viewing: bool,
        mut ui: impl FnMut(
            InteractiveState<'_>,
            Option<Presentation>,
        ) -> Result<ControlFlow<Reason>, ()>
        + 'a,
        mut result: impl FnMut(ResultEvent) + 'a,
        mut other: impl FnMut(Route, &[u8]) -> Result<Disposition, ()> + 'a,
    ) -> impl Future<Output = Result<InteractiveCloseOutcome, Error>> + 'a {
        let viewing = self.prepare_interactive(channels, request, policy);
        let fence = self.control.clone();
        let local_stop = self.control.clone();
        let operation = Operation { viewer: self };
        Guarded {
            fence,
            inner: Box::pin(async move {
                let operation = operation;
                operation.viewer.install_interactive(viewing?)?;
                let mut closing = None;
                let mut viewing_close = None;
                let mut before_grant = false;
                let result = operation
                    .viewer
                    .serve_inner(
                        &mut |state, event| {
                            let decision = match state {
                                Dispatch::Existing(State::Controlled(input)) => {
                                    match ui(InteractiveState::Controlled(&mut *input), event)? {
                                        ControlFlow::Continue(()) => return Ok(()),
                                        ControlFlow::Break(reason) => {
                                            closing = Some(
                                                input
                                                    .begin_disconnect_with_cleanup(cleanup, reason),
                                            );
                                            return Err(());
                                        }
                                    }
                                }
                                Dispatch::Viewing(viewing) => {
                                    match ui(InteractiveState::Viewing(&mut *viewing), event)? {
                                        ControlFlow::Break(reason)
                                            if close_viewing && viewing.requested.is_none() =>
                                        {
                                            // notify cannot promote a request after
                                            // this Err, and there was none before it.
                                            viewing_close = Some(reason);
                                            return Err(());
                                        }
                                        decision => decision,
                                    }
                                }
                                Dispatch::Existing(State::Requesting(pending)) => {
                                    ui(InteractiveState::Requesting(pending), event)?
                                }
                                Dispatch::Existing(State::Observing) => return Err(()),
                            };
                            match decision {
                                ControlFlow::Continue(()) => Ok(()),
                                ControlFlow::Break(_) => {
                                    // Notify has not promoted or sent a newly staged
                                    // request yet. Cancel before any native unwind;
                                    // never guess whether the peer granted a lease.
                                    local_stop.stop();
                                    before_grant = true;
                                    Err(())
                                }
                            }
                        },
                        &mut result,
                        &mut other,
                    )
                    .await;
                match (result, closing, viewing_close) {
                    (Err(Error::Application), Some(closing), _) => {
                        operation.viewer.close_media();
                        let outcome = closing.await.map_err(Error::Control)?;
                        operation.viewer.record_control_disconnect(outcome)?;
                        Ok(InteractiveCloseOutcome::Controlled(outcome))
                    }
                    (Err(Error::Application), None, Some(reason)) => operation
                        .viewer
                        .disconnect_unrequested(reason)
                        .await
                        .map(InteractiveCloseOutcome::Viewing),
                    (Err(Error::Application), None, None) if before_grant => Err(Error::Closed),
                    (Err(error), _, _) => Err(error),
                    _ => Err(Error::Closed),
                }
            }),
        }
    }

    /// No mutable application callback runs between validating the original
    /// unrequested state and fencing its ordinary session I/O. A failed or
    /// abandoned exchange still closes this owner and retains native custody.
    fn disconnect_unrequested(
        &mut self,
        reason: Reason,
    ) -> impl Future<Output = Result<CloseOutcome, Error>> + '_ {
        let ending = Operation { viewer: self };
        let prepared = match &mut ending.viewer.peer {
            Peer::Viewing {
                session, viewing, ..
            } if viewing.requested.is_none() => {
                acquisition::validate(session, &viewing.channels, viewing.request)
                    .map(|()| session.begin_disconnect_before_control(reason))
            }
            _ => Err(Error::RequestAlreadyStarted),
        };
        if prepared.is_err() {
            ending.viewer.control.stop();
            ending.viewer.peer.close();
        }
        ending.viewer.close_media();
        async move {
            let outcome = prepared?.await.map_err(Error::Session)?;
            ending.viewer.disconnect_outcome = Some(outcome);
            if let Peer::Viewing { session, .. } = &mut ending.viewer.peer
                && let Some(report) = outcome.report
            {
                session.remote_closed = Some(report);
            }
            drop(ending);
            Ok(outcome)
        }
    }
}
