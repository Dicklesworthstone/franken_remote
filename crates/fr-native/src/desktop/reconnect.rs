//! Desktop reconnection through the canonical installed-tailnet supervisor.
//! Each attempt owns a fresh window, decoder, input queue and clipboard. Only
//! positively completed native cleanup permits another attempt; no replay.
#![forbid(unsafe_code)]
use super::{CaptureCleanup, Cleanup, Configuration, Desktop, Error, PickerCleanup, WindowCleanup};
use asupersync::{cx::Cx, time::sleep_until, types::Time};
use fr_client::{input, startup::ApprovalNotice};
use fr_core::input_submission::Capabilities;
use fr_media::freshness::ClockPolicy;
use fr_wire::display::Catalog;
use frd::{
    native_connection::reconnect::{Application, CallbackError, Status},
    session_startup::{
        InteractiveViewerState, ObserverError, ObserverPolicy, Presentation,
        StreamingViewerControl, Viewer, viewer_events::Layout,
    },
    worker::Deadline,
};

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

/// Bounded synchronous callbacks on the session thread. No callback may block
/// on native work. The observer's original mapping/visibility requirements apply
/// unchanged; a map notification or decoder receipt is not a visibility proof.
pub trait Ui {
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
    desktop: Option<Desktop>,
    opened: bool,
    last_attempt: u8,
    error: Option<Error>,
    cleanup: Option<Cleanup>,
    cleanup_failure: Option<CleanupFailure>,
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
            desktop: None,
            opened: false,
            last_attempt: 0,
            error: None,
            cleanup: None,
            cleanup_failure: None,
        }
    }
    pub fn desktop(&self) -> Option<&Desktop> {
        self.desktop.as_ref()
    }
    pub const fn last_error(&self) -> Option<Error> {
        self.error
    }
    pub const fn last_cleanup(&self) -> Option<&Cleanup> {
        self.cleanup.as_ref()
    }
    pub const fn cleanup_failure(&self) -> Option<CleanupFailure> {
        self.cleanup_failure
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
            configuration.display_picker = base.display_picker;
            configuration.fit_window = base.fit_window;
            configuration
        })
        .map_err(|_| ObserverError::Application)
    }
}
impl<U: Ui> Application for Session<U> {
    type Output = ();
    async fn run(&mut self, attempt: u8, viewer: Viewer) -> Result<(), ObserverError> {
        let configuration = self.configuration(attempt)?;
        let stop = viewer.control();
        self.last_attempt = attempt;
        self.opened = false;
        self.error = None;
        self.cleanup = None;
        self.cleanup_failure = None;
        // Store before ANY await or callback so interrupted work stays owned.
        self.desktop = Some(Desktop::new(configuration));
        let desktop = self.desktop.as_mut().ok_or(ObserverError::Order)?;
        let ui = std::cell::RefCell::new(&mut self.ui);
        let clock = match self.mode {
            Mode::Observe => None,
            Mode::ControlCapable(policy) => Some(policy.clock),
        };
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
            callback(&stop, || ui.borrow_mut().ready(attempt, desktop))
                .map_err(|_| Error::Observer(ObserverError::Application))?;
            match self.mode {
                Mode::Observe => desktop
                    .serve(|frame| {
                        callback(&stop, || ui.borrow_mut().frame(attempt, frame)).map_err(|_| ())
                    })?
                    .await
                    .map_err(|e| Error::Observer(ObserverError::Streaming(e))),
                Mode::ControlCapable(policy) => {
                    desktop
                        .serve_interactive(
                            policy.sequence,
                            policy.capabilities,
                            policy.input,
                            |state, frame| {
                                callback(&stop, || {
                                    ui.borrow_mut().interactive(attempt, state, frame)
                                })
                                .map_err(|_| ())
                            },
                            |result| {
                                let _ = callback(&stop, || {
                                    ui.borrow_mut().input_result(attempt, result);
                                    Ok(())
                                });
                            },
                        )?
                        .await
                }
            }
        }
        .await;
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
            desktop.close();
        }
        async move {
            let result = self.finish_cleanup(cx, deadline).await;
            self.cleanup_failure = result.err();
            result.map_err(|_| CallbackError)
        }
    }
    fn status(&mut self, status: Status) -> Result<(), CallbackError> {
        match self
            .desktop
            .as_ref()
            .and_then(|desktop| desktop.stop.as_ref())
        {
            Some(stop) => callback(stop, || self.ui.status(status)),
            None => self.ui.status(status),
        }
    }
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
