//! Desktop reconnection through the canonical installed-tailnet supervisor.
//! Each attempt owns a fresh window, decoder, input queue and clipboard. Only
//! positively completed native cleanup permits another attempt; no replay.
#![forbid(unsafe_code)]
mod once;
use super::{CaptureCleanup, Cleanup, Configuration, Desktop, Error, PickerCleanup, WindowCleanup};
use asupersync::{
    cx::Cx,
    runtime::RuntimeHandle,
    time::sleep_until,
    types::{Budget, Time},
};
use fr_client::{input, startup::ApprovalNotice};
use fr_core::input_submission::Capabilities;
use fr_media::freshness::ClockPolicy;
use fr_wire::{display::Catalog, negotiation::Offer};
use frd::{
    native_connection::reconnect::{Application, CallbackError, CompletionAction, Status},
    session_startup::{
        InteractiveViewerState, ObserverError, ObserverPolicy, Presentation,
        StreamingViewerControl, Viewer, ViewerCloseOutcome, ViewerStatistics,
        viewer_events::Layout,
    },
    worker::Deadline,
};
pub use once::Observation;

/// Control-capable means viewing until a NEW explicit UI request. These are
/// policies and an initial sequence, never retained credentials or actions.
#[derive(Debug, Clone, Copy)]
pub struct ControlPolicy {
    pub clock: ClockPolicy,
    pub input: input::Policy,
    pub sequence: u64,
    pub capabilities: Capabilities,
}
#[derive(Debug, Clone, Copy)]
pub enum Mode {
    Observe,
    ControlCapable(ControlPolicy),
}

/// Content-free evidence sampled from the original attempt after its native
/// cleanup finishes (successfully or otherwise). It carries no reusable owner,
/// input authority, layout, ticket or file permission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CleanupSummary {
    pub disconnect: Option<ViewerCloseOutcome>,
    pub control_disconnect: Option<frd::session_startup::ControlCloseOutcome>,
    pub input_stop: Option<crate::viewer_input::StopReason>,
    pub failure: Option<CleanupFailure>,
}
impl CleanupSummary {
    /// Adverse original-owner evidence still vetoes an automatic viewing retry.
    /// A checked host layout change alone permits fresh viewing after cleanup;
    /// it is never permission to renew the old input lease or replay actions.
    pub fn retry_failure(self) -> Option<ObserverError> {
        retained_failure(self, true).filter(|error| {
            !frd::native_connection::reconnect::geometry_changed(
                frd::native_connection::reconnect::Failure::Observation(*error),
            )
        })
    }
    pub fn prevents_viewing_retry(self) -> bool {
        use crate::viewer_input::StopReason;
        self.failure.is_some()
            || self
                .input_stop
                .is_some_and(|reason| !matches!(reason, StopReason::Local | StopReason::Closed))
            || self.retry_failure().is_some()
    }
}

/// Bounded synchronous callbacks on the session thread. No callback may block
/// on native work. The observer's original mapping/visibility requirements apply
/// unchanged; a map notification or decoder receipt is not a visibility proof.
pub trait Ui {
    /// Narrow this fresh attempt's advertisement by omitting optional
    /// capabilities whose original local choice has already been spent.
    /// The canonical supervisor rejects any other change. This callback runs
    /// before discovery/dial and creates no desktop, input or auxiliary owner.
    fn offer(&mut self, _attempt: u8, original: &Offer) -> Result<Offer, CallbackError> {
        Ok(original.clone())
    }
    fn choose(&mut self, attempt: u8, catalog: &Catalog) -> Result<Option<u128>, CallbackError>;
    fn approval(&mut self, attempt: u8, notice: ApprovalNotice) -> Result<(), CallbackError>;
    /// Retain content-free window/clipboard handles and configure this NEW
    /// original controller-only clipboard here. Never carry old authority.
    fn ready(&mut self, _attempt: u8, _desktop: &mut Desktop) -> Result<(), CallbackError> {
        Ok(())
    }
    fn frame(&mut self, _attempt: u8, _frame: Option<Presentation>) -> Result<(), CallbackError> {
        Err(CallbackError)
    }
    /// Only this callback may explicitly request control on Viewing. Return a
    /// confirmed renderer-matching Layout once Controlled to attach original input.
    fn interactive(
        &mut self,
        _attempt: u8,
        _state: InteractiveViewerState<'_>,
        _frame: Option<Presentation>,
    ) -> Result<Option<Layout>, CallbackError> {
        Err(CallbackError)
    }
    fn input_result(&mut self, _attempt: u8, _result: input::ResultEvent) {}
    /// Informational: never changes the cleanup result. A local application
    /// can retain original terminal reports and veto a later Reconnecting
    /// notification when a genuine input stop raced with transport failure.
    fn cleanup_finished(&mut self, _attempt: u8, _summary: CleanupSummary) {}
    fn status(&mut self, _status: Status) -> Result<(), CallbackError> {
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanupFailure {
    /// Bootstrap did not return the original supervised decoder owner. Dropping
    /// it cannot be promoted to proof that every spawned worker was reaped.
    BootstrapUnconfirmed,
    Media,
    Files,
    Clipboard,
    Expired,
    Cancelled,
    Clock,
}

/// Pass this retained application to `Client::run_observing` or, for the matching
/// Mode, `Client::run_control_capable`. That supervisor supplies fresh `LocalAPI`,
/// TLS and identity checks, bounded backoff and an independent cleanup context.
/// This adapter never dials, retries, approves, or takes control by itself.
/// Keep it after failure to inspect or finish cleanup of the original Desktop.
pub struct Session<U> {
    configuration: Configuration,
    policy: ObserverPolicy,
    mode: Mode,
    ui: U,
    one_shot: bool,
    display_switching: bool,
    select_fresh_display: bool,
    /// Original native intent only; this handle cannot create another owner.
    display_switch_window: Option<crate::viewer_window::WindowControl>,
    initial: Option<Observation>,
    initial_snapshot: Option<fr_media::worker::presentation_snapshot::Snapshot>,
    desktop: Option<Desktop>,
    opened: bool,
    last_attempt: u8,
    current_attempt: u8,
    service_succeeded: bool,
    error: Option<Error>,
    cleanup: Option<Cleanup>,
    cleanup_failure: Option<CleanupFailure>,
    disconnect: Option<ViewerCloseOutcome>,
    /// Where a granted controller's closing exchange gets its independent
    /// cleanup context. Without it a controller's window close stops
    /// immediately, with no closing exchange (the original behaviour).
    cleanup_runtime: Option<RuntimeHandle>,
    control_disconnect: Option<frd::session_startup::ControlCloseOutcome>,
    statistics: Option<ViewerStatistics>,
    /// Why the last attempt's local input capture stopped, sampled before
    /// cleanup closes it. Content-free; a capture stop otherwise reaches the
    /// session only as a cancelled scope.
    input_stop: Option<crate::viewer_input::StopReason>,
}
impl<U> Session<U> {
    pub const fn new(
        configuration: Configuration,
        policy: ObserverPolicy,
        mode: Mode,
        ui: U,
    ) -> Self {
        Self {
            configuration,
            policy,
            mode,
            ui,
            one_shot: false,
            display_switching: false,
            select_fresh_display: false,
            display_switch_window: None,
            initial: None,
            initial_snapshot: None,
            desktop: None,
            opened: false,
            last_attempt: 0,
            current_attempt: 0,
            service_succeeded: false,
            error: None,
            cleanup: None,
            cleanup_failure: None,
            disconnect: None,
            cleanup_runtime: None,
            control_disconnect: None,
            statistics: None,
            input_stop: None,
        }
    }
    /// Observe exactly one initial native completion, then run the original
    /// viewer's `InspectionComplete` closing exchange and mandatory cleanup.
    /// This constructor cannot request control. The caller still chooses the
    /// supervisor's finite attempt policy; use one attempt for a one-shot CLI.
    pub fn observing_once(configuration: Configuration, policy: ObserverPolicy, ui: U) -> Self {
        let mut session = Self::new(configuration, policy, Mode::Observe, ui);
        session.one_shot = true;
        session
    }
    /// Historical first-frame evidence, retained through cleanup. This is not
    /// current source freshness, physical visibility, pixels, or input authority.
    /// A failed run or incomplete cleanup must not be rendered as success.
    pub const fn initial_observation(&self) -> Option<Observation> {
        self.initial
    }
    /// Consume the explicitly requested artifact after inspecting the original
    /// run and cleanup result. This never reacquires or refreshes observation.
    pub fn take_initial_snapshot(
        &mut self,
    ) -> Option<fr_media::worker::presentation_snapshot::Snapshot> {
        self.initial_snapshot.take()
    }
    /// Give a control-capable session an orderly window close: a controller's
    /// window-manager close runs the terminal exchange on a fresh context
    /// from `runtime` instead of dropping the connection.
    #[must_use]
    pub fn with_cleanup_runtime(mut self, runtime: RuntimeHandle) -> Self {
        self.cleanup_runtime = Some(runtime);
        self
    }
    /// Permit a trusted local change-display gesture. Every accepted change
    /// closes and reaps the original owner before authenticating a fresh
    /// session and showing its fresh native display catalog. A control-capable
    /// caller must also supply `with_cleanup_runtime`; one-shot observations
    /// never enable the gesture.
    #[must_use]
    pub fn with_display_switching(mut self) -> Self {
        self.display_switching = true;
        self
    }
    pub fn desktop(&self) -> Option<&Desktop> {
        self.desktop.as_ref()
    }
    pub const fn last_error(&self) -> Option<Error> {
        self.error
    }
    /// The last attempt's first published local input-capture stop reason,
    /// retained through native cleanup (never a host or transport result).
    pub const fn last_input_stop(&self) -> Option<crate::viewer_input::StopReason> {
        self.input_stop
    }
    pub const fn last_cleanup(&self) -> Option<&Cleanup> {
        self.cleanup.as_ref()
    }
    pub const fn cleanup_failure(&self) -> Option<CleanupFailure> {
        self.cleanup_failure
    }
    /// Original closing exchange retained after the Desktop's native owners
    /// are reaped. None is no completed exchange, never confirmed remote cleanup.
    pub const fn disconnect_outcome(&self) -> Option<ViewerCloseOutcome> {
        self.disconnect
    }
    /// A granted controller's closing exchange (session report and any lease
    /// report). None is no completed exchange, never confirmed cleanup.
    pub const fn control_disconnect_outcome(
        &self,
    ) -> Option<frd::session_startup::ControlCloseOutcome> {
        self.control_disconnect
    }
    /// The last attempt's content-free viewer counters, sampled before its
    /// native owners are reaped. None: that attempt never opened a viewer.
    pub const fn statistics(&self) -> Option<ViewerStatistics> {
        self.statistics
    }
    fn configuration(&self, attempt: u8) -> Result<Configuration, ObserverError> {
        // Same finite bound as the canonical supervisor. Monotonic attempts and
        // checked epoch addition prevent worker generation reuse, even at MAX.
        if !(1..=32).contains(&attempt) || attempt <= self.last_attempt || self.desktop.is_some() {
            return Err(ObserverError::Order);
        }
        let base = &self.configuration;
        let epoch = base
            .worker_epoch
            .checked_add(u128::from(attempt - 1))
            .ok_or(ObserverError::Order)?;
        Configuration::new(
            &base.image,
            &base.display,
            base.xauthority.as_deref(),
            epoch,
        )
        .map(|mut configuration| {
            configuration.display_picker = base.display_picker || self.select_fresh_display;
            configuration.fit_window = base.fit_window;
            configuration.clipboard_space = base.clipboard_space;
            configuration.capture_system_shortcuts = base.capture_system_shortcuts;
            configuration.initial_snapshot = base.initial_snapshot;
            configuration
        })
        .map_err(|_| ObserverError::Application)
    }
    fn prepare_attempt(&mut self, attempt: u8) -> Result<(), ObserverError> {
        let configuration = self.configuration(attempt)?;
        self.initial = None;
        self.initial_snapshot = None;
        self.last_attempt = attempt;
        self.current_attempt = attempt;
        self.service_succeeded = false;
        self.opened = false;
        self.error = None;
        self.cleanup = None;
        self.cleanup_failure = None;
        self.disconnect = None;
        self.control_disconnect = None;
        self.statistics = None;
        self.input_stop = None;
        self.display_switch_window = None;
        // Store before ANY await or callback so interrupted work stays owned.
        self.desktop = Some(Desktop::new(configuration));
        Ok(())
    }
}
/// A control-capable attempt's service: an orderly window close when a
/// cleanup context exists, otherwise the original immediate close.
async fn serve_control(
    desktop: &mut Desktop,
    cleanup: Option<&Cx>,
    policy: ControlPolicy,
    interactive: impl FnMut(
        InteractiveViewerState<'_>,
        Option<Presentation>,
    ) -> Result<Option<Layout>, ()>,
    result: impl FnMut(input::ResultEvent),
) -> Result<(), Error> {
    match cleanup {
        Some(cleanup) => desktop
            .serve_interactive_until(
                cleanup,
                policy.sequence,
                policy.capabilities,
                policy.input,
                interactive,
                result,
            )?
            .await
            .map(|_| ()),
        None => {
            desktop
                .serve_interactive(
                    policy.sequence,
                    policy.capabilities,
                    policy.input,
                    interactive,
                    result,
                )?
                .await
        }
    }
}
impl<U: Ui> Application for Session<U> {
    type Output = ();
    fn offer(&mut self, attempt: u8, original: &Offer) -> Result<Offer, CallbackError> {
        if !(1..=32).contains(&attempt) || attempt <= self.last_attempt || self.desktop.is_some() {
            return Err(CallbackError);
        }
        self.ui.offer(attempt, original)
    }
    async fn run(&mut self, attempt: u8, viewer: Viewer) -> Result<(), ObserverError> {
        self.prepare_attempt(attempt)?;
        let stop = viewer.control();
        let desktop = self.desktop.as_mut().ok_or(ObserverError::Order)?;
        let ui = std::cell::RefCell::new(&mut self.ui);
        let clock = match self.mode {
            Mode::Observe => None,
            Mode::ControlCapable(policy) => Some(policy.clock),
        };
        let cleanup = match (&self.mode, &self.cleanup_runtime) {
            (Mode::ControlCapable(_), Some(runtime)) => Some(
                runtime
                    .try_request_cx_with_budget(Budget::INFINITE)
                    .map_err(|_| ObserverError::Application)?,
            ),
            _ => None,
        };
        let one_shot = self.one_shot;
        let display_switching = self.display_switching && !one_shot;
        let outcome = async {
            desktop
                .open(
                    viewer,
                    self.policy,
                    clock,
                    |catalog| {
                        callback(&stop, || ui.borrow_mut().choose(attempt, catalog)).map_err(|_| ())
                    },
                    |notice| {
                        callback(&stop, || ui.borrow_mut().approval(attempt, notice))
                            .map_err(|_| ())
                    },
                )?
                .await?;
            self.opened = true;
            self.initial_snapshot = desktop.take_initial_snapshot();
            if one_shot {
                let observer = desktop.observer.as_ref().ok_or(Error::NotViewing)?;
                self.initial = Some(Observation {
                    display: observer.display(),
                    completion: observer.initial_presentation(),
                });
            }
            if display_switching {
                if matches!(self.mode, Mode::ControlCapable(_)) && cleanup.is_none() {
                    return Err(Error::Observer(ObserverError::Order));
                }
                desktop.enable_display_switching()?;
                self.display_switch_window = desktop.window();
            }
            callback(&stop, || ui.borrow_mut().ready(attempt, desktop))
                .map_err(|_| Error::Observer(ObserverError::Application))?;
            match self.mode {
                Mode::Observe => desktop
                    .serve_until(|frame| {
                        once::flow(one_shot, || {
                            callback(&stop, || ui.borrow_mut().frame(attempt, frame))
                        })
                        .map_err(|_| ())
                    })?
                    .await
                    .map(|_| ())
                    .map_err(|e| Error::Observer(ObserverError::Streaming(e))),
                Mode::ControlCapable(policy) => {
                    serve_control(
                        desktop,
                        cleanup.as_ref(),
                        policy,
                        |state, frame| {
                            callback(&stop, || ui.borrow_mut().interactive(attempt, state, frame))
                                .map_err(|_| ())
                        },
                        |result| {
                            let _ = callback(&stop, || {
                                ui.borrow_mut().input_result(attempt, result);
                                Ok(())
                            });
                        },
                    )
                    .await
                }
            }
        }
        .await;
        self.service_succeeded = outcome.is_ok();
        self.disconnect = desktop.disconnect_outcome();
        self.control_disconnect = desktop.control_disconnect_outcome();
        self.statistics = desktop.statistics();
        // Record genuine native cancellation BEFORE cleanup can stop a pending
        // picker itself. It cannot relabel a deadline/link failure as user intent.
        let selection_cancelled = outcome.is_err() && desktop.cancelled_selection();
        self.error = if selection_cancelled {
            Some(Error::Picker(crate::display_picker::Error::Cancelled))
        } else {
            outcome.as_ref().err().copied()
        };
        outcome.map_err(|error| match error {
            Error::Observer(error) => error,
            _ => ObserverError::Application,
        })
    }
    fn cleanup(
        &mut self,
        cx: &Cx,
        deadline: Deadline,
    ) -> impl std::future::Future<Output = Result<(), CallbackError>> {
        // Fence at CALL time, including abandoned/unpolled cleanup futures.
        if let Some(desktop) = &mut self.desktop {
            // A stop can drop the attempt's future before its own sample; the
            // counters only grow, so this one is at least as complete.
            if let Some(statistics) = desktop.statistics() {
                self.statistics = Some(statistics);
            }
            // The attempt future may have been abandoned after receiving a
            // terminal report but before copying it into Session. Preserve the
            // original owner's evidence before close/reap can remove it.
            self.disconnect = desktop.disconnect_outcome().or(self.disconnect);
            self.control_disconnect = desktop
                .control_disconnect_outcome()
                .or(self.control_disconnect);
            // Sample BEFORE close: closing records its own local stop.
            if let Some(crate::viewer_input::Status::Stopped(reason)) =
                desktop.input().map(|input| input.status())
            {
                self.input_stop = Some(reason);
            }
            desktop.close();
        }
        async move {
            let result = self.finish_cleanup(cx, deadline).await;
            self.cleanup_failure = result.err();
            if self.last_attempt != 0 {
                self.ui.cleanup_finished(
                    self.last_attempt,
                    CleanupSummary {
                        disconnect: self.disconnect,
                        control_disconnect: self.control_disconnect,
                        input_stop: self.input_stop,
                        failure: self.cleanup_failure,
                    },
                );
            }
            result.map_err(|_| CallbackError)
        }
    }
    fn status(&mut self, status: Status) -> Result<(), CallbackError> {
        if let Status::Connecting { attempt } = status {
            self.current_attempt = attempt;
        }
        match self
            .desktop
            .as_ref()
            .and_then(|desktop| desktop.stop.as_ref())
        {
            Some(stop) => callback(stop, || self.ui.status(status)),
            None => self.ui.status(status),
        }
    }
    fn completion_action(&mut self) -> Result<CompletionAction, CallbackError> {
        let Some(window) = &self.display_switch_window else {
            return Ok(CompletionAction::Finish);
        };
        if !window.display_switch_requested() || window.close_requested() {
            return Ok(CompletionAction::Finish);
        }
        // The supervisor invokes this only after successful service and
        // positive native cleanup. Recheck the retained original evidence;
        // an unrelated stop or adverse host report cannot become a new dial.
        if !self.display_switching
            || self.one_shot
            || !self.service_succeeded
            || self.desktop.is_some()
            || self.error.is_some()
            || !requested_close_allows_reopen(CleanupSummary {
                disconnect: self.disconnect,
                control_disconnect: self.control_disconnect,
                input_stop: self.input_stop,
                failure: self.cleanup_failure,
            })
        {
            return Err(CallbackError);
        }
        self.select_fresh_display = true;
        self.display_switch_window = None;
        Ok(CompletionAction::Reopen)
    }
    fn terminal_failure(&self) -> Option<ObserverError> {
        // A failed later discovery cannot acquire the previous native owner's
        // report. Original reports still remain available through their accessors.
        if self.current_attempt != self.last_attempt {
            return None;
        }
        retained_failure(
            CleanupSummary {
                disconnect: self.disconnect,
                control_disconnect: self.control_disconnect,
                input_stop: self.input_stop,
                failure: self.cleanup_failure,
            },
            !self.service_succeeded,
        )
    }
}

/// A completed bounded exchange can retain unknown transport/remote cleanup
/// evidence. A fresh viewing session does not claim release or replay input;
/// known host failures, revoked permission and failed cleanup veto reopening.
fn requested_close_allows_reopen(summary: CleanupSummary) -> bool {
    use crate::viewer_input::StopReason;
    summary.failure.is_none()
        && !summary
            .input_stop
            .is_some_and(|reason| !matches!(reason, StopReason::Local | StopReason::Closed))
        && (summary.disconnect.is_some() || summary.control_disconnect.is_some())
        && closing_failure(summary).is_none()
}

/// Preserve exact adverse reports/errors when the completion callback refuses.
/// An expected own-close report cannot relabel a later protocol/auth failure.
fn closing_failure(summary: CleanupSummary) -> Option<ObserverError> {
    retained_failure(summary, false)
}

// A failed attempt cannot treat ClientRequested/SessionEnded as its own
// successful closing exchange. Consider every retained report before allowing
// a geometry retry: a separate permission, protocol or cleanup failure wins.
fn retained_failure(summary: CleanupSummary, failed: bool) -> Option<ObserverError> {
    use fr_wire::{closure, lease_revoked};
    use frd::session_startup::{ControlledViewerError, StreamingViewerError};
    let lease = summary
        .control_disconnect
        .and_then(|outcome| outcome.revocation)
        .filter(|report| {
            failed
                || !matches!(
                    report.reason,
                    lease_revoked::Reason::ClientRequested | lease_revoked::Reason::SessionEnded
                )
                || report.cleanup == lease_revoked::CleanupStage::Failed
        })
        .map(|report| {
            ObserverError::Streaming(StreamingViewerError::Control(
                ControlledViewerError::LeaseRevoked(report),
            ))
        });
    let exchanges = [
        summary.disconnect,
        summary.control_disconnect.map(|outcome| outcome.exchange),
    ];
    let reports = exchanges
        .into_iter()
        .flatten()
        .filter_map(|outcome| outcome.report)
        .filter(|report| {
            failed
                || report.reason != closure::ClosedReason::ClientRequested
                || report.cleanup == closure::Cleanup::Incomplete
        })
        .map(|report| ObserverError::Session(frd::session_startup::Error::RemoteClosed(report)));
    let transport = exchanges.into_iter().flatten().filter_map(|outcome| {
        outcome
            .transport
            .err()
            .filter(|error| {
                !matches!(
                    error,
                    fr_transport::quic::Error::Expired
                        | fr_transport::quic::Error::Closed
                        | fr_transport::quic::Error::Backpressure
                )
            })
            .map(|error| ObserverError::Streaming(StreamingViewerError::Transport(error)))
    });
    let mut geometry = None;
    for error in lease.into_iter().chain(reports).chain(transport) {
        if frd::native_connection::reconnect::geometry_changed(
            frd::native_connection::reconnect::Failure::Observation(error),
        ) {
            geometry.get_or_insert(error);
        } else {
            return Some(error);
        }
    }
    geometry
}
impl<U> Session<U> {
    async fn finish_cleanup(&mut self, cx: &Cx, deadline: Deadline) -> Result<(), CleanupFailure> {
        let Some(desktop) = &mut self.desktop else {
            return Ok(());
        };
        let mut previous = cleanup_clock(cx, deadline, None)?;
        self.cleanup = Some(desktop.reap(cx, deadline).await);
        loop {
            previous = cleanup_clock(cx, deadline, Some(previous))?;
            let report = self.cleanup.as_mut().ok_or(CleanupFailure::Media)?;
            report.input = desktop.observer.as_mut().map_or(
                CaptureCleanup::NotStarted,
                frd::session_startup::NativeObserver::input_capture_cleanup,
            );
            // Native cancellation can wake this runtime before the capture
            // thread publishes its retained first cause. Once that same owner
            // is reaped, sample its original handle again before discarding it.
            // A reason observed before cleanup remains authoritative here.
            if report.input == CaptureCleanup::Complete
                && self.input_stop.is_none()
                && let Some(crate::viewer_input::Status::Stopped(reason)) =
                    desktop.input().map(|input| input.status())
            {
                self.input_stop = Some(reason);
            }
            report.window = desktop.window_cleanup();
            report.picker = desktop.picker_cleanup();
            if cleaned(report)? {
                if !self.opened {
                    // Desktop retains the one-shot Launch's cleanup custody
                    // before it can escape to startup. Its successful media
                    // receipt now proves either no child started or that the
                    // exact retired child was reaped, even before open returned.
                    if report.input == CaptureCleanup::NotStarted
                        && matches!(
                            report.clipboard,
                            Ok(frd::native_clipboard::Cleanup::NotStarted)
                        )
                    {
                        self.desktop = None;
                        return Ok(());
                    }
                    return Err(CleanupFailure::BootstrapUnconfirmed);
                }
                if !matches!(report.media, Ok(Some(_))) {
                    return Err(CleanupFailure::Media);
                }
                self.desktop = None;
                self.opened = false;
                return Ok(());
            }
            // Poll only nonblocking native reaps. Do not restart media/clipboard
            // cleanup or reset its deadline while a native thread is draining.
            let wake = Time::from_nanos(previous.as_nanos().saturating_add(1_000_000));
            sleep_until(wake.min(deadline.time())).await;
        }
    }
}
fn cleanup_clock(
    cx: &Cx,
    deadline: Deadline,
    previous: Option<Time>,
) -> Result<Time, CleanupFailure> {
    cx.checkpoint().map_err(|_| CleanupFailure::Cancelled)?;
    let now = cx.timer_driver().ok_or(CleanupFailure::Clock)?.now();
    if previous.is_some_and(|before| now < before) {
        return Err(CleanupFailure::Clock);
    }
    if now >= deadline.time() {
        return Err(CleanupFailure::Expired);
    }
    Ok(now)
}
fn cleaned(report: &Cleanup) -> Result<bool, CleanupFailure> {
    if report.media.is_err() {
        return Err(CleanupFailure::Media);
    }
    if report.files.is_err() {
        return Err(CleanupFailure::Files);
    }
    let clipboard = match report.clipboard {
        Ok(
            frd::native_clipboard::Cleanup::NotStarted
            | frd::native_clipboard::Cleanup::Finished(_),
        ) => true,
        Ok(frd::native_clipboard::Cleanup::Pending) => false,
        Err(_) => return Err(CleanupFailure::Clipboard),
    };
    Ok(clipboard
        && report.input != CaptureCleanup::Pending
        && report.window != WindowCleanup::Pending
        && report.picker != PickerCleanup::Pending)
}
// A caught UI panic must fence NOW, not when the retained async future is dropped.
fn callback<T>(
    stop: &StreamingViewerControl,
    action: impl FnOnce() -> Result<T, CallbackError>,
) -> Result<T, CallbackError> {
    struct Fence<'a>(&'a StreamingViewerControl, bool);
    impl Drop for Fence<'_> {
        fn drop(&mut self) {
            if !self.1 {
                self.0.stop();
            }
        }
    }
    let mut fence = Fence(stop, false);
    let result = action();
    fence.1 = result.is_ok();
    result
}

#[cfg(test)]
mod tests;
