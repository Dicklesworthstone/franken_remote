//! Original native observer, input attachment and interactive closing owner.
use super::{Attempt, Cx, Error, NativeObserver, streaming};
use fr_client::input::{Policy, ResultEvent};
use fr_core::input_submission::Capabilities;
use fr_transport::ControlCloseOutcome;
use fr_wire::closure::Reason;
use std::{future::Future, ops::ControlFlow};

impl NativeObserver {
    /// Continue the original native control-capable bootstrap through watching,
    /// a local control request, actual grant, and an explicit terminal exchange.
    /// Neither an input attachment nor watching reserves a lease or input Seat.
    /// Break before a grant stops locally; it never guesses a lease from intent.
    ///
    /// `cleanup` is independently provisioned on the same runtime clock. After
    /// grant, the original input owner fences before native decoder retirement.
    /// Native window/signal emergency paths remain immediate; this is an explicit
    /// bounded UI policy, not an automatic upgrade of window-close behavior.
    pub fn serve_interactive_control_until<'a>(
        &'a mut self,
        cleanup: &'a Cx,
        sequence: u64,
        capabilities: Capabilities,
        policy: Policy,
        ui: impl FnMut(
            streaming::InteractiveState<'_>,
            Option<streaming::Presentation>,
        ) -> Result<ControlFlow<Reason>, ()>
        + 'a,
        result: impl FnMut(ResultEvent) + 'a,
    ) -> impl Future<Output = Result<ControlCloseOutcome, Error>> + 'a {
        let cx = self.cx.clone();
        let request = self.control_request(sequence, capabilities);
        let future = request.and_then(|request| {
            self.input
                .take()
                .ok_or(Error::InvalidConfiguration)
                .map(|input| {
                    self.viewer.serve_interactive_control_until(
                        input,
                        request,
                        policy,
                        cleanup,
                        ui,
                        result,
                        |_, _| Err(()),
                    )
                })
        });
        Attempt {
            cx,
            complete: false,
            inner: Box::pin(async move { future?.await.map_err(Error::Streaming) }),
        }
    }

    /// Read the original controller's exact completed terminal exchange after
    /// shutdown or reaping. No report and unknown effects stay distinct from zero.
    pub fn control_disconnect_outcome(&self) -> Option<ControlCloseOutcome> {
        self.viewer.control_disconnect_outcome()
    }
    /// Retained input ledger, including encoded actions discarded without sends.
    /// Cleanup and host terminal reports cannot fabricate per-action receipts.
    pub fn pending_control_actions(&self) -> usize {
        self.viewer.pending_control_actions()
    }
}
