//! Closing policy on the original interactive watch/request/grant service.
use super::{
    Cx, Dispatch, Error, Guarded, InteractiveState, NegotiatedInput, Operation, Policy,
    Presentation, Request, ResultEvent, State, StreamingViewer,
};
use fr_transport::{
    ControlCloseOutcome,
    quic::{Disposition, Route},
};
use fr_wire::closure::Reason;
use std::{future::Future, ops::ControlFlow};

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
        mut ui: impl FnMut(
            InteractiveState<'_>,
            Option<Presentation>,
        ) -> Result<ControlFlow<Reason>, ()>
        + 'a,
        mut result: impl FnMut(ResultEvent) + 'a,
        mut other: impl FnMut(Route, &[u8]) -> Result<Disposition, ()> + 'a,
    ) -> impl Future<Output = Result<ControlCloseOutcome, Error>> + 'a {
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
                                    ui(InteractiveState::Viewing(viewing), event)?
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
                match (result, closing) {
                    (Err(Error::Application), Some(closing)) => {
                        operation.viewer.close_media();
                        let outcome = closing.await.map_err(Error::Control)?;
                        operation.viewer.record_control_disconnect(outcome)?;
                        Ok(outcome)
                    }
                    (Err(Error::Application), None) if before_grant => Err(Error::Closed),
                    (Err(error), _) => Err(error),
                    _ => Err(Error::Closed),
                }
            }),
        }
    }
}
