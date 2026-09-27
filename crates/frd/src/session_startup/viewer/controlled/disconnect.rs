//! Close the actual granted controller without replaying or erasing input effects.
use super::{ControlledViewer, Cx, Error, Operation, Role, StopReason};
use fr_transport::ControlCloseOutcome;
use fr_wire::{
    authority::Binding,
    closure::{CloseRequest, Reason},
};
use std::future::Future;

const CLOSE_BUDGET_MICROS: u64 = 250_000;

impl ControlledViewer {
    /// Fence this granted controller's input and native capture at CALL time,
    /// then request the original host's terminal report on the original socket.
    /// There is no new event source, input grant, receipt identity or connection.
    /// Media/presentation owners must be stopped separately before this call.
    ///
    /// `cleanup` must have been independently provisioned on the same runtime
    /// clock before application shutdown. Fencing input cancels its application
    /// context; that context is never reset or un-cancelled. Supplying the same
    /// cancelled context refuses the exchange but still ends local input. The
    /// cleanup context's own cancellation and immutable transport security apply.
    ///
    /// Close discards unsent bytes, NOT the original pending-action ledger or
    /// last receipt. An acknowledged request or reported release cannot resolve
    /// missing per-action results. The first exact session or lease report is
    /// returned and retained independently of native-capture cleanup. A blocked
    /// native producer remains owned for `reap_input_capture`; stop is not reap.
    ///
    /// Every outcome, invalid attempt, preparation unwind and unpolled abandonment
    /// ends ordinary I/O. The existing 250-ms construction-time cap is shortened
    /// by original response, silence and pending-record deadlines, not restarted
    /// after cleanup. Native transport backlog refuses instead of flushing input.
    pub fn disconnect_with_cleanup(
        &mut self,
        cleanup: &Cx,
        reason: Reason,
    ) -> impl Future<Output = Result<ControlCloseOutcome, Error>> + '_ {
        let ending = Operation {
            viewer: self,
            complete: false,
        };
        let viewer = &mut *ending.viewer;
        // No native adapter runs before the scope/role/budget are captured and
        // ordinary method admission is fenced. A failed preflight is terminal.
        let ready = (|| {
            if viewer.is_closed() {
                return Err(Error::Closed);
            }
            viewer.session.check()?;
            if viewer.session.opened.selection.role != Role::RequestControl {
                return Err(Error::WrongBinding);
            }
            // Freeze before any native stop callback, not after it returns.
            let call_until = viewer
                .session
                .last
                .checked_add(CLOSE_BUDGET_MICROS)
                .ok_or(Error::Expired)?;
            let until = viewer
                .session
                .responder
                .response_deadline()
                .map_or(viewer.session.heard_until, |at| {
                    at.0.min(viewer.session.heard_until)
                });
            let until = until.min(call_until);
            let until = viewer
                .input
                .control_response_deadline()
                .map_or(until, |at| until.min(at.0));
            Ok(viewer
                .pending
                .as_ref()
                .map_or(until, |p| until.min(p.until)))
        })();
        let lease = viewer.input.binding().lease;
        viewer.session.closed = true;
        viewer.session.responder.stop();
        if let Some(clock) = &mut viewer.session.clock {
            clock.stop();
        }
        viewer.fence_input();
        let prepared = ready.map(|until| {
            Box::pin(viewer.session.transport.close_control_with_request(
                cleanup,
                &viewer.session.connection,
                viewer.session.routes,
                Binding {
                    channel: viewer.session.opened.binding.id,
                    session: viewer.session.opened.binding.remote_session,
                },
                lease,
                CloseRequest { reason },
                until,
            ))
        });
        if prepared.is_err() {
            viewer.session.close();
        }
        async move {
            let outcome = prepared?.await;
            if let Some(report) = outcome.exchange.report {
                ending.viewer.session.remote_closed = Some(report);
            }
            ending.viewer.disconnect_outcome = Some(outcome);
            drop(ending);
            Ok(outcome)
        }
    }

    /// Exact completed terminal exchange, preserved through repeated close and
    /// native reaping. None includes unpolled abandonment, not successful cleanup.
    pub const fn disconnect_outcome(&self) -> Option<ControlCloseOutcome> {
        self.disconnect_outcome
    }

    /// Input admission and receipt retention are independent. Establish the fence
    /// before invoking any native capture, clipboard or file cleanup boundary.
    pub(super) fn fence_input(&mut self) {
        self.control.stop();
        self.input.stop(StopReason::Disconnected);
        self.pending = None;
        self.events = None;
        self.clock.stop();
        let _ = self.files.stop(&mut self.session.transport);
        if let Some(capture) = &self.native_capture {
            capture.stop();
        }
        if let Some(clipboard) = &self.clipboard {
            clipboard.stop();
        }
        self.clipboard_setup.stop();
        self.stop_local_cursor();
        self.viewport.stop();
    }
}
