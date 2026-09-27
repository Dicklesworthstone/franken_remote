//! Ordered observation retirement on the existing receiver and closing exchange.
use super::{Error, Guarded, Operation, Peer, Presentation, StreamingViewer};
use fr_transport::quic::{CloseOutcome, Disposition};
use fr_wire::{closure::Reason, negotiation::Role};
use std::{cell::Cell, future::Future, ops::ControlFlow};

impl StreamingViewer {
    /// Stop this observation's native media before preparing its one close
    /// exchange. Ordinary I/O is closed at CALL time, even for an unpolled future.
    /// The original decoder remains owned and can be reaped through `reap_media`;
    /// abort/request ACK/host cleanup reports do not certify its OS exit.
    ///
    /// A controller or control-intent viewer refuses and uses ordinary terminal
    /// teardown; input receipts and native cleanup obligations are not bypassed.
    /// The original session cancellation handle still interrupts the exchange.
    pub fn disconnect(
        &mut self,
        reason: Reason,
    ) -> impl Future<Output = Result<CloseOutcome, Error>> + '_ {
        let ending = Operation { viewer: self };
        let viewer = &mut *ending.viewer;
        let eligible = viewer.observation_only();
        // No callback or foreign work can see a still-live receiving scope after
        // media retirement. Actual native ownership is retained for explicit reap.
        if !eligible {
            viewer.control.stop();
            viewer.peer.close();
        }
        viewer.close_media();
        let prepared = match &mut viewer.peer {
            Peer::Observe { session, .. } if eligible => Ok(session.begin_disconnect(reason)),
            _ => Err(Error::Session(super::super::Error::Order)),
        };
        if prepared.is_err() {
            viewer.control.stop();
            viewer.peer.close();
        }
        async move {
            let outcome = prepared?.await.map_err(Error::Session)?;
            ending.viewer.disconnect_outcome = Some(outcome);
            if let Peer::Observe { session, .. } = &mut ending.viewer.peer
                && let Some(report) = outcome.report
            {
                session.remote_closed = Some(report);
            }
            drop(ending);
            Ok(outcome)
        }
    }

    /// Exact completed exchange, retained through later local cleanup. None
    /// means no completed attempt, including abandonment. A transport error may
    /// still contain a valid host report; absence never means zero effects.
    pub const fn disconnect_outcome(&self) -> Option<CloseOutcome> {
        self.disconnect_outcome
    }

    /// Run the ordinary observation loop until the bounded UI callback chooses
    /// Break(reason), then stop media and exchange the original `CloseRequest`.
    /// Continue(()) does not grant input or certify visibility. Callback failure,
    /// emergency stop, transport failure and abandonment remain terminal errors.
    ///
    /// The decision is handled at an existing completed network-turn boundary,
    /// including while a native decode is pending. That decode is fenced/aborted
    /// before closing, not awaited or completed as a new presentation receipt.
    /// During recovery, existing generation/transport guards may already have
    /// retired the connection; closing then refuses rather than reviving it.
    /// There is no second receiver, decoder, transport or packet queue.
    pub fn serve_until<'a>(
        &'a mut self,
        mut ui: impl FnMut(Option<Presentation>) -> Result<ControlFlow<Reason>, ()> + 'a,
    ) -> impl Future<Output = Result<CloseOutcome, Error>> + 'a {
        let fence = self.control.clone();
        let operation = Operation { viewer: self };
        Guarded {
            fence,
            inner: Box::pin(async move {
                let operation = operation;
                if !operation.viewer.observation_only() {
                    return Err(Error::Session(super::super::Error::Order));
                }
                let request = Cell::new(None);
                let result = operation
                    .viewer
                    .serve_inner(
                        &mut |_, event| match ui(event)? {
                            ControlFlow::Continue(()) => Ok(()),
                            ControlFlow::Break(reason) => {
                                request.set(Some(reason));
                                Err(()) // Exit only after the current network turn.
                            }
                        },
                        &mut |_| {},
                        &mut |_, _| Ok(Disposition::Blocked),
                    )
                    .await;
                // Only our explicit UI decision selects orderly closure. Never
                // reinterpret an unrelated application/protocol failure as intent.
                match (result, request.get()) {
                    (Err(Error::Application), Some(reason)) => {
                        operation.viewer.disconnect(reason).await
                    }
                    (Err(error), _) => Err(error),
                    _ => Err(Error::Closed),
                }
            }),
        }
    }

    pub(in crate::session_startup::viewer) fn observation_only(&self) -> bool {
        matches!(&self.peer, Peer::Observe { session, .. }
            if session.opened.selection.role == Role::Observe)
    }

    /// Independent of session cancellation so the already-fenced observation can
    /// retain its original socket ONLY for the typed terminal exchange.
    pub(super) fn close_media(&mut self) {
        self.receiver.close();
        self.presenter.abort();
        self.repair.clear();
        if let Some(recovery) = &mut self.recovery {
            recovery.close();
        }
        self.initial = None;
        self.cursor = None;
        if let Some(audio) = &mut self.audio {
            audio.close();
        }
        if let Some(app) = &mut self.clipboard {
            app.close();
        }
    }
}
