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
            observer
                .reap_media(cx, deadline)
                .await
                .map_err(|_| CallbackError)?;
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
        (self.status)(status)
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
        observer: None,
        started: false,
    }
}
struct NativeControl<L, C, A, U, R, S> {
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
    observer: Option<NativeObserver>,
    started: bool,
}
impl<L, C, A, U, R, S> Application for NativeControl<L, C, A, U, R, S>
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
{
    type Output = ();
    async fn run(&mut self, attempt: u8, viewer: Viewer) -> Result<(), ObserverError> {
        if self.started || self.observer.is_some() {
            return Err(ObserverError::Order);
        }
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
            observer
                .reap_media(cx, deadline)
                .await
                .map_err(|_| CallbackError)?;
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
        (self.status)(status)
    }
}
