//! Fence the granted input owner before abandoning native decode or closing I/O.
use super::super::{
    ControlledViewer, Cx, Error, Guarded, Operation, Peer, Presentation, StreamingViewer,
};
use fr_client::input::ResultEvent;
use fr_transport::{
    ControlCloseOutcome,
    quic::{Disposition, Route},
};
use fr_wire::closure::Reason;
use std::{future::Future, ops::ControlFlow};

impl StreamingViewer {
    /// End an actually granted controller on its original connection. Input is
    /// fenced and the original deadlines are frozen BEFORE any native media is
    /// retired. Receiver, audio and pending decode work stop at method call;
    /// borrowed compressed pictures stay charged until their owners release them.
    /// The original native input and decoder owners remain available for reap.
    ///
    /// `cleanup` is an independently provisioned context on the same runtime
    /// clock, not the application context cancelled by the input fence. It grants
    /// no input or observation. Wrong peer states close locally without emitting
    /// a fabricated lease report. Observation-only closing has its separate API.
    pub fn disconnect_control_with_cleanup(
        &mut self,
        cleanup: &Cx,
        reason: Reason,
    ) -> impl Future<Output = Result<ControlCloseOutcome, Error>> + '_ {
        let ending = Operation { viewer: self };
        let prepared = match &mut ending.viewer.peer {
            Peer::Control(viewer) => Some(viewer.begin_disconnect_with_cleanup(cleanup, reason)),
            _ => None,
        };
        if prepared.is_none() {
            ending.viewer.control.stop();
            ending.viewer.peer.close();
        }
        ending.viewer.close_media();
        async move {
            let outcome = prepared
                .ok_or(Error::Closed)?
                .await
                .map_err(Error::Control)?;
            ending.viewer.record_control_disconnect(outcome)?;
            drop(ending);
            Ok(outcome)
        }
    }

    /// Exact original lease/session reports, never per-action completion or
    /// local native cleanup. Retained even after close or reaping either owner.
    pub fn control_disconnect_outcome(&self) -> Option<ControlCloseOutcome> {
        match &self.peer {
            Peer::Control(viewer) => viewer.disconnect_outcome(),
            _ => None,
        }
    }

    /// Pending actions remain unresolved after their unsent bytes are discarded.
    /// This query never replays work or manufactures receipts from a host report.
    pub fn pending_control_actions(&self) -> usize {
        match &self.peer {
            Peer::Control(viewer) => viewer.pending_actions(),
            _ => 0,
        }
    }

    /// Service an already-granted controller until its bounded UI explicitly
    /// requests closing. The input fence and terminal preparation run INSIDE that
    /// callback turn, before a pending decoder future can be dropped. No further
    /// UI, input-result, media or application callbacks run during the exchange.
    /// Callback failure, emergency stop and protocol failure remain immediate
    /// terminal errors; only Break selects the closing-only exchange.
    pub fn serve_control_until<'a>(
        &'a mut self,
        cleanup: &'a Cx,
        mut ui: impl FnMut(
            &mut ControlledViewer,
            Option<Presentation>,
        ) -> Result<ControlFlow<Reason>, ()>
        + 'a,
        mut result: impl FnMut(ResultEvent) + 'a,
        mut other: impl FnMut(Route, &[u8]) -> Result<Disposition, ()> + 'a,
    ) -> impl Future<Output = Result<ControlCloseOutcome, Error>> + 'a {
        let fence = self.control.clone();
        let operation = Operation { viewer: self };
        Guarded {
            fence,
            inner: Box::pin(async move {
                let operation = operation;
                if !matches!(operation.viewer.peer, Peer::Control(_)) {
                    return Err(Error::Closed);
                }
                let mut closing = None;
                let result = operation
                    .viewer
                    .serve_inner(
                        &mut |state, event| {
                            let viewer = state.controlled().ok_or(())?;
                            match ui(viewer, event)? {
                                ControlFlow::Continue(()) => Ok(()),
                                ControlFlow::Break(reason) => {
                                    closing =
                                        Some(viewer.begin_disconnect_with_cleanup(cleanup, reason));
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
                    (Err(error), _) => Err(error),
                    _ => Err(Error::Closed),
                }
            }),
        }
    }

    pub(super) fn record_control_disconnect(
        &mut self,
        outcome: ControlCloseOutcome,
    ) -> Result<(), Error> {
        let Peer::Control(viewer) = &mut self.peer else {
            return Err(Error::Closed);
        };
        viewer.record_disconnect(outcome);
        Ok(())
    }
}
