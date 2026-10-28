//! Native viewing applications with retained, explicitly reaped decoder ownership.
use super::{Application, CallbackError, Status};
use crate::{
    session_startup::{
        InteractiveViewerState, NativeObserver, ObserverError, ObserverPolicy, Presentation, Viewer,
    },
    worker::{Deadline, Launch},
};
use asupersync::cx::Cx;
use fr_client::{
    input::{Policy as InputPolicy, ResultEvent},
    startup::ApprovalNotice,
};
use fr_core::input_submission::Capabilities;
use fr_media::freshness::ClockPolicy;
use fr_wire::display::Catalog;

/// Build a native application for `Client::run_observing`. Every attempt uses
/// the original approved observer bootstrap and a locally supplied worker launch.
/// Display choice and approval notification are repeated for the NEW session;
/// no old alias, decoder reference or presentation is carried across attempts.
///
/// The adapter retains the complete native observer when its service ends or
/// renewal interrupts it. Cleanup reaps its worker before permitting a retry.
/// Interrupted bootstrap without a returned cleanup owner is terminal, never
/// an assertion that dropping a future proved native reaping. UI Err or explicit
/// supervisor cancellation stops rather than requesting another connection.
pub fn native_view(
    policy: ObserverPolicy,
    launch: impl FnMut(u8) -> Result<Launch, CallbackError>,
    choose: impl FnMut(u8, &Catalog) -> Result<Option<u128>, CallbackError>,
    approval: impl FnMut(u8, ApprovalNotice) -> Result<(), CallbackError>,
    ui: impl FnMut(u8, Option<Presentation>) -> Result<(), CallbackError>,
    status: impl FnMut(Status) -> Result<(), CallbackError>,
) -> impl Application<Output = ()> {
    Native {
        policy,
        launch,
        choose,
        approval,
        ui,
        status,
        observer: None,
        started: false,
        terminal: None,
    }
}
struct Native<L, C, A, U, S> {
    policy: ObserverPolicy,
    launch: L,
    choose: C,
    approval: A,
    ui: U,
    status: S,
    observer: Option<NativeObserver>,
    started: bool,
    terminal: Option<ObserverError>,
}
impl<L, C, A, U, S> Application for Native<L, C, A, U, S>
where
    L: FnMut(u8) -> Result<Launch, CallbackError>,
    C: FnMut(u8, &Catalog) -> Result<Option<u128>, CallbackError>,
    A: FnMut(u8, ApprovalNotice) -> Result<(), CallbackError>,
    U: FnMut(u8, Option<Presentation>) -> Result<(), CallbackError>,
    S: FnMut(Status) -> Result<(), CallbackError>,
{
    type Output = ();
    async fn run(&mut self, attempt: u8, viewer: Viewer) -> Result<(), ObserverError> {
        if self.started || self.observer.is_some() {
            return Err(ObserverError::Order);
        }
        self.terminal = None;
        let launch = (self.launch)(attempt).map_err(|_| ObserverError::Application)?;
        self.started = true;
        self.observer = Some(
            viewer
                .observe(
                    launch,
                    self.policy,
                    |catalog| (self.choose)(attempt, catalog).map_err(|_| ()),
                    |notice| (self.approval)(attempt, notice).map_err(|_| ()),
                )
                .await?,
        );
        let initial = self
            .observer
            .as_ref()
            .ok_or(ObserverError::Order)?
            .initial_presentation();
        (self.ui)(attempt, Some(initial)).map_err(|_| ObserverError::Application)?;
        self.observer
            .as_mut()
            .ok_or(ObserverError::Order)?
            .serve(|frame| (self.ui)(attempt, frame).map_err(|_| ()))
            .await
            .map_err(ObserverError::Streaming)
    }
    async fn cleanup(&mut self, cx: &Cx, deadline: Deadline) -> Result<(), CallbackError> {
        if let Some(observer) = self.observer.as_mut() {
            // Capture only this owner's validated explicit reports before
            // fencing/reaping can discard it. Cleanup itself creates no report.
            self.terminal = self.terminal.or_else(|| reported_terminal(observer));
            observer.close(); // Fence input/publication before either foreign cleanup.
            let media = observer.reap_media(cx, deadline).await;
            let input = observer.reap_input_capture(cx, deadline).await;
            let clipboard = observer.reap_clipboard(cx, deadline).await;
            let files = observer.reap_files(cx, deadline).await;
            media.map_err(|_| CallbackError)?;
            input.map_err(|_| CallbackError)?;
            clipboard_finished(clipboard.map_err(|_| CallbackError)?)?;
            files.map_err(|_| CallbackError)?;
            self.observer = None;
            self.started = false;
            Ok(())
        } else if self.started {
            Err(CallbackError)
        } else {
            Ok(())
        }
    }
    fn status(&mut self, status: Status) -> Result<(), CallbackError> {
        if matches!(status, Status::Connecting { .. }) {
            if self.started || self.observer.is_some() {
                return Err(CallbackError);
            }
            self.terminal = None;
        }
        (self.status)(status)
    }
    fn terminal_failure(&self) -> Option<ObserverError> {
        self.terminal
    }
}

/// Build a native application for `Client::run_control_capable`. Every retry
/// reconstructs only a fresh control-capable *viewing* session. The returned UI
/// begins in `InteractiveViewerState::Viewing`; it must explicitly call
/// `ViewingControl::request_control` after fresh mapping/visibility evidence.
/// Neither this adapter nor the reconnect supervisor carries an old request,
/// lease, ticket, held state, receipt sequence, approval, or media reference.
#[allow(clippy::too_many_arguments)] // Independent UI callbacks and qualified policies, as in run_control_capable.
pub fn native_control_view(
    observer_policy: ObserverPolicy,
    clock_policy: ClockPolicy,
    input_policy: InputPolicy,
    sequence: u64,
    capabilities: Capabilities,
    launch: impl FnMut(u8) -> Result<Launch, CallbackError>,
    choose: impl FnMut(u8, &Catalog) -> Result<Option<u128>, CallbackError>,
    approval: impl FnMut(u8, ApprovalNotice) -> Result<(), CallbackError>,
    ui: impl for<'v> FnMut(
        u8,
        InteractiveViewerState<'v>,
        Option<Presentation>,
    ) -> Result<(), CallbackError>,
    result: impl FnMut(u8, ResultEvent),
    status: impl FnMut(Status) -> Result<(), CallbackError>,
) -> impl Application<Output = ()> {
    native_control_view_with_setup(
        observer_policy,
        clock_policy,
        input_policy,
        sequence,
        capabilities,
        launch,
        choose,
        approval,
        ui,
        result,
        status,
        |_, _| Ok(()),
    )
}

/// Configure each NEW, approved native observer before its interactive service.
/// Use this callback for independent clipboard consent/factory configuration and
/// to retain that attempt's content-free clipboard UI handle. It runs once per
/// fresh observer, never on a failed connection or by reusing an old session.
/// No control request is generated by the adapter. Callback failure retains the
/// native owner for cleanup; a blocked clipboard worker prevents another attempt.
#[allow(clippy::too_many_arguments)] // Independent UI callbacks and qualified policies, as in run_control_capable.
pub fn native_control_view_with_setup(
    observer_policy: ObserverPolicy,
    clock_policy: ClockPolicy,
    input_policy: InputPolicy,
    sequence: u64,
    capabilities: Capabilities,
    launch: impl FnMut(u8) -> Result<Launch, CallbackError>,
    choose: impl FnMut(u8, &Catalog) -> Result<Option<u128>, CallbackError>,
    approval: impl FnMut(u8, ApprovalNotice) -> Result<(), CallbackError>,
    ui: impl for<'v> FnMut(
        u8,
        InteractiveViewerState<'v>,
        Option<Presentation>,
    ) -> Result<(), CallbackError>,
    result: impl FnMut(u8, ResultEvent),
    status: impl FnMut(Status) -> Result<(), CallbackError>,
    setup: impl FnMut(u8, &mut NativeObserver) -> Result<(), CallbackError>,
) -> impl Application<Output = ()> {
    NativeControl {
        observer_policy,
        clock_policy,
        input_policy,
        sequence,
        capabilities,
        launch,
        choose,
        approval,
        ui,
        result,
        status,
        setup,
        observer: None,
        started: false,
        terminal: None,
    }
}
struct NativeControl<L, C, A, U, R, S, P> {
    observer_policy: ObserverPolicy,
    clock_policy: ClockPolicy,
    input_policy: InputPolicy,
    sequence: u64,
    capabilities: Capabilities,
    launch: L,
    choose: C,
    approval: A,
    ui: U,
    result: R,
    status: S,
    setup: P,
    observer: Option<NativeObserver>,
    started: bool,
    terminal: Option<ObserverError>,
}
impl<L, C, A, U, R, S, P> Application for NativeControl<L, C, A, U, R, S, P>
where
    L: FnMut(u8) -> Result<Launch, CallbackError>,
    C: FnMut(u8, &Catalog) -> Result<Option<u128>, CallbackError>,
    A: FnMut(u8, ApprovalNotice) -> Result<(), CallbackError>,
    U: for<'v> FnMut(
        u8,
        InteractiveViewerState<'v>,
        Option<Presentation>,
    ) -> Result<(), CallbackError>,
    R: FnMut(u8, ResultEvent),
    S: FnMut(Status) -> Result<(), CallbackError>,
    P: FnMut(u8, &mut NativeObserver) -> Result<(), CallbackError>,
{
    type Output = ();
    async fn run(&mut self, attempt: u8, viewer: Viewer) -> Result<(), ObserverError> {
        if self.started || self.observer.is_some() {
            return Err(ObserverError::Order);
        }
        self.terminal = None;
        let launch = (self.launch)(attempt).map_err(|_| ObserverError::Application)?;
        self.started = true;
        self.observer = Some(
            viewer
                .observe_for_control(
                    launch,
                    self.observer_policy,
                    self.clock_policy,
                    |catalog| (self.choose)(attempt, catalog).map_err(|_| ()),
                    |notice| (self.approval)(attempt, notice).map_err(|_| ()),
                )
                .await?,
        );
        let observer = self.observer.as_mut().ok_or(ObserverError::Order)?;
        (self.setup)(attempt, observer).map_err(|_| ObserverError::Application)?;
        observer
            .serve_interactive_control(
                self.sequence,
                self.capabilities,
                self.input_policy,
                |state, frame| (self.ui)(attempt, state, frame).map_err(|_| ()),
                |event| (self.result)(attempt, event),
            )
            .await
    }
    async fn cleanup(&mut self, cx: &Cx, deadline: Deadline) -> Result<(), CallbackError> {
        if let Some(observer) = self.observer.as_mut() {
            // Capture only this owner's validated explicit reports before
            // fencing/reaping can discard it. Cleanup itself creates no report.
            self.terminal = self.terminal.or_else(|| reported_terminal(observer));
            observer.close(); // Fence input/publication before either foreign cleanup.
            let media = observer.reap_media(cx, deadline).await;
            let input = observer.reap_input_capture(cx, deadline).await;
            let clipboard = observer.reap_clipboard(cx, deadline).await;
            let files = observer.reap_files(cx, deadline).await;
            media.map_err(|_| CallbackError)?;
            input.map_err(|_| CallbackError)?;
            clipboard_finished(clipboard.map_err(|_| CallbackError)?)?;
            files.map_err(|_| CallbackError)?;
            self.observer = None;
            self.started = false;
            Ok(())
        } else if self.started {
            Err(CallbackError)
        } else {
            Ok(())
        }
    }
    fn status(&mut self, status: Status) -> Result<(), CallbackError> {
        if matches!(status, Status::Connecting { .. }) {
            if self.started || self.observer.is_some() {
                return Err(CallbackError);
            }
            self.terminal = None;
        }
        (self.status)(status)
    }
    fn terminal_failure(&self) -> Option<ObserverError> {
        self.terminal
    }
}

fn reported_terminal(observer: &NativeObserver) -> Option<ObserverError> {
    explicit_terminal(
        observer.disconnect_outcome(),
        observer.control_disconnect_outcome(),
    )
}
/// These getters describe completed explicit exchanges only. Ordinary peer
/// errors keep their exact typed payload through the native/network join.
fn explicit_terminal(
    viewing: Option<fr_transport::quic::CloseOutcome>,
    control: Option<fr_transport::ControlCloseOutcome>,
) -> Option<ObserverError> {
    use crate::session_startup::{
        ControlledViewerError as C, Error as S, StreamingViewerError as V,
    };
    if let Some(report) = control.and_then(|outcome| outcome.revocation) {
        return Some(ObserverError::Streaming(V::Control(C::LeaseRevoked(
            report,
        ))));
    }
    viewing
        .and_then(|outcome| outcome.report)
        .or_else(|| control.and_then(|outcome| outcome.exchange.report))
        .map(|report| ObserverError::Streaming(V::Session(S::RemoteClosed(report))))
}

fn clipboard_finished(cleanup: crate::native_clipboard::Cleanup) -> Result<(), CallbackError> {
    use crate::native_clipboard::Cleanup;
    match cleanup {
        // Finished contains the worker's OPERATION outcome; either outcome
        // proves its thread was joined. Do not confuse cancellation of work
        // with an uncollected native owner or replay the failed operation.
        Cleanup::NotStarted | Cleanup::Finished(_) => Ok(()),
        Cleanup::Pending => Err(CallbackError),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn report() -> fr_wire::closure::Closed {
        use fr_wire::closure::{Cleanup, Closed, ClosedReason, OutstandingEffects};
        Closed {
            reason: ClosedReason::PermissionLost,
            cleanup: Cleanup::Unconfirmed,
            effects: OutstandingEffects::Known {
                pending: 5,
                uncertain: 2,
            },
        }
    }
    fn terminal() -> ObserverError {
        ObserverError::Streaming(crate::session_startup::StreamingViewerError::Session(
            crate::session_startup::Error::RemoteClosed(report()),
        ))
    }
    fn observing(started: bool) -> impl Application<Output = ()> {
        Native {
            policy: ObserverPolicy::default(),
            launch: |_: u8| -> Result<Launch, CallbackError> { panic!("unexpected native launch") },
            choose: |_: u8, _: &Catalog| Ok(None),
            approval: |_: u8, _: ApprovalNotice| Ok(()),
            ui: |_: u8, _: Option<Presentation>| Ok(()),
            status: |_: Status| Ok(()),
            observer: None,
            started,
            terminal: Some(terminal()),
        }
    }
    fn controlled(started: bool) -> impl Application<Output = ()> {
        NativeControl {
            observer_policy: ObserverPolicy::default(),
            clock_policy: ClockPolicy::default(),
            input_policy: InputPolicy::default(),
            sequence: 1,
            capabilities: Capabilities::default(),
            launch: |_: u8| -> Result<Launch, CallbackError> { panic!("unexpected native launch") },
            choose: |_: u8, _: &Catalog| Ok(None),
            approval: |_: u8, _: ApprovalNotice| Ok(()),
            ui: |_: u8, _: InteractiveViewerState<'_>, _: Option<Presentation>| Ok(()),
            result: |_: u8, _: ResultEvent| {},
            status: |_: Status| Ok(()),
            setup: |_: u8, _: &mut NativeObserver| Ok(()),
            observer: None,
            started,
            terminal: Some(terminal()),
        }
    }
    fn completed_latch(mut application: impl Application<Output = ()>) {
        // Exercise the actual adapters without native I/O: the fixture report
        // is metadata, not proof that an authenticated peer or worker existed.
        let rt = asupersync::runtime::RuntimeBuilder::current_thread()
            .enable_platform_reactor(true)
            .build()
            .unwrap();
        let cx = rt.request_cx_with_budget(asupersync::types::Budget::INFINITE);
        let deadline = Deadline::after(&cx, std::time::Duration::from_secs(1)).unwrap();
        assert_eq!(application.terminal_failure(), Some(terminal()));
        assert_eq!(rt.block_on(application.cleanup(&cx, deadline)), Ok(()));
        assert_eq!(application.terminal_failure(), Some(terminal()));
        application
            .status(Status::Connecting { attempt: 2 })
            .unwrap();
        assert_eq!(application.terminal_failure(), None);
        // Failed discovery starts no observer. Repeated cleanup cannot carry
        // the preceding attempt's report into this one.
        assert_eq!(rt.block_on(application.cleanup(&cx, deadline)), Ok(()));
        assert_eq!(application.terminal_failure(), None);
    }
    fn unfinished_latch(mut application: impl Application<Output = ()>) {
        assert_eq!(
            application.status(Status::Connecting { attempt: 2 }),
            Err(CallbackError)
        );
        assert_eq!(application.terminal_failure(), Some(terminal()));
    }
    #[test]
    fn native_adapters_retain_terminal_reports_until_a_fresh_attempt_after_cleanup() {
        completed_latch(observing(false));
        completed_latch(controlled(false));
        unfinished_latch(observing(true));
        unfinished_latch(controlled(true));
    }
    #[test]
    fn explicit_exchange_reports_preserve_exact_effects_and_do_not_invent_revocation() {
        use fr_wire::lease_revoked::{CleanupStage, EffectStage, Reason, Revoked};
        let partial = fr_transport::quic::CloseOutcome {
            request_acknowledged: true,
            report: None,
            transport: Err(fr_transport::quic::Error::Expired),
        };
        assert_eq!(explicit_terminal(Some(partial), None), None);
        assert_eq!(
            explicit_terminal(
                None,
                Some(fr_transport::ControlCloseOutcome {
                    exchange: partial,
                    revocation: None,
                })
            ),
            None
        );
        let exchange = fr_transport::quic::CloseOutcome {
            report: Some(report()),
            ..partial
        };
        assert_eq!(explicit_terminal(Some(exchange), None), Some(terminal()));
        assert_eq!(
            explicit_terminal(
                Some(partial),
                Some(fr_transport::ControlCloseOutcome {
                    exchange,
                    revocation: None,
                })
            ),
            Some(terminal())
        );
        let revoked = Revoked {
            lease: fr_core::ids::InputLeaseId::from_raw(412),
            reason: Reason::LocalRevoke,
            cleanup: CleanupStage::Fenced,
            effects: EffectStage::ReceiptsPending,
        };
        assert_eq!(
            explicit_terminal(
                None,
                Some(fr_transport::ControlCloseOutcome {
                    exchange: partial,
                    revocation: Some(revoked),
                })
            ),
            Some(ObserverError::Streaming(
                crate::session_startup::StreamingViewerError::Control(
                    crate::session_startup::ControlledViewerError::LeaseRevoked(revoked),
                )
            ))
        );
    }
    #[test]
    fn cleanup_is_not_success_just_because_the_clipboard_call_returned() {
        use crate::native_clipboard::Cleanup;
        for state in [
            Cleanup::NotStarted,
            Cleanup::Finished(Ok(())),
            Cleanup::Finished(Err(crate::clipboard_quic::Error::Closed)),
        ] {
            assert_eq!(clipboard_finished(state), Ok(()));
        }
        assert_eq!(clipboard_finished(Cleanup::Pending), Err(CallbackError));
    }
}
