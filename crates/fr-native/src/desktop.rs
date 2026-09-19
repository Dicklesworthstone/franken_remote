//! One locally owned desktop: original session, selected window and decoder.
//!
//! This composes the existing authenticated Viewer; it opens no listener, dials
//! no unqualified transport, invents no visibility witness, and never retries a
//! failed session. Every native owner remains available for explicit cleanup.
#![forbid(unsafe_code)]
pub mod reconnect;
use crate::{
    display_picker::{self, DisplayPicker},
    viewer_window::{self, ViewerWindow, WindowControl},
};
use asupersync::{cx::Cx, process::ExitStatus};
use fr_media::{freshness::ClockPolicy, worker::Role};
use fr_wire::display::{Catalog, Display};
use frd::{
    session_startup::{
        NativeObserver, ObserverError, ObserverPolicy, Presentation, StreamingViewerControl,
        StreamingViewerError, Viewer, ViewerStatistics, viewer_events::CaptureCleanup,
    },
    worker::{Deadline, Launch, Retirement},
};
use std::{
    cell::Cell,
    fmt,
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    rc::Rc,
    task::{Context, Poll},
};

/// Local package and graphical-session configuration. This cannot be supplied
/// by a remote peer. Absolute paths are not a package-signature/trust check.
pub struct Configuration {
    image: PathBuf,
    display: String,
    xauthority: Option<PathBuf>,
    worker_epoch: u128,
    display_picker: bool,
}
impl Configuration {
    pub fn new(
        image: &Path,
        display: &str,
        xauthority: Option<&Path>,
        worker_epoch: u128,
    ) -> Result<Self, Error> {
        Launch::new(image, display, xauthority, Role::Present, worker_epoch)
            .map_err(Error::Launch)?;
        Ok(Self {
            image: image.into(),
            display: display.into(),
            xauthority: xauthority.map(Path::to_path_buf),
            worker_epoch,
            display_picker: false,
        })
    }
    /// Use a fresh native choice surface in THIS approved selection exchange.
    /// The callback passed to `open` is not used to choose in this mode. No
    /// catalog/handle/choice is retained across attempts or defaults to primary.
    #[must_use]
    pub const fn with_display_picker(mut self) -> Self {
        self.display_picker = true;
        self
    }
}
impl fmt::Debug for Configuration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DesktopConfiguration([local package and graphical session])")
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    New,
    Opening,
    Viewing,
    Stopped,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    AlreadyUsed,
    NotViewing,
    Launch(frd::worker::Error),
    Window(viewer_window::Error),
    Picker(display_picker::Error),
    Observer(ObserverError),
    Clipboard(frd::clipboard_quic::Error),
    InputCapture(
        frd::session_startup::viewer_events::CaptureStartError<crate::viewer_input::Error>,
    ),
    CaptureAlreadyStarted,
    LayoutMismatch,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Error {}

/// Separate outcomes: none of these is evidence of host-held-key release or
/// physical pixel erasure. A failed media reap does not discard the worker.
#[derive(Debug)]
pub struct Cleanup {
    pub media: Result<Option<ExitStatus>, frd::media::Error>,
    pub input: CaptureCleanup,
    pub window: WindowCleanup,
    pub picker: PickerCleanup,
    pub clipboard: Result<frd::native_clipboard::Cleanup, frd::clipboard_quic::Error>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowCleanup {
    NotStarted,
    Pending,
    Complete(viewer_window::StopReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerCleanup {
    NotStarted,
    Pending,
    Complete,
}

/// One attempt only. Keep this owner even when `open` fails so native cleanup is
/// collectable. Reconnect uses a NEW Desktop and freshly authenticated Viewer;
/// it never reuses this window, input queue, decoder or control grant.
#[must_use]
pub struct Desktop {
    configuration: Configuration,
    state: State,
    observer: Option<NativeObserver>,
    window: Option<ViewerWindow>,
    picker: Option<DisplayPicker>,
    renderer_started: bool,
    retirement: Option<Retirement>,
    stop: Option<StreamingViewerControl>,
    input: Option<crate::viewer_input::CaptureControl>,
}
impl Desktop {
    pub const fn new(configuration: Configuration) -> Self {
        Self {
            configuration,
            state: State::New,
            observer: None,
            window: None,
            picker: None,
            renderer_started: false,
            retirement: None,
            stop: None,
            input: None,
        }
    }
    pub fn state(&self) -> State {
        if self
            .stop
            .as_ref()
            .is_some_and(StreamingViewerControl::is_stopped)
        {
            State::Stopped
        } else {
            self.state
        }
    }
    pub fn window(&self) -> Option<WindowControl> {
        self.window.as_ref().map(ViewerWindow::control)
    }
    /// Status/cancellation for the original attempt's native chooser. Consumed
    /// handles are retired and cannot stop the later viewing window.
    pub fn picker(&self) -> Option<display_picker::Control> {
        self.picker.as_ref().map(DisplayPicker::control)
    }
    pub fn picker_cleanup(&mut self) -> PickerCleanup {
        self.picker
            .as_mut()
            .map_or(PickerCleanup::NotStarted, |picker| {
                if picker.finish() {
                    PickerCleanup::Complete
                } else {
                    PickerCleanup::Pending
                }
            })
    }
    pub(super) fn cancelled_selection(&self) -> bool {
        !self.renderer_started
            && self.picker().is_some_and(|picker| {
                picker.status() == display_picker::Status::Stopped(display_picker::Error::Cancelled)
            })
    }
    pub fn display(&self) -> Option<Display> {
        self.observer.as_ref().map(NativeObserver::display)
    }
    pub fn statistics(&self) -> Option<ViewerStatistics> {
        self.observer.as_ref().map(NativeObserver::statistics)
    }
    pub fn worker_id(&self) -> Option<u32> {
        self.observer.as_ref().and_then(NativeObserver::worker_id)
    }
    /// The native capture handle belongs to the ORIGINAL granted viewer, not
    /// this facade. This is content-free status/stop, never an alternate queue.
    pub fn input(&self) -> Option<crate::viewer_input::CaptureControl> {
        self.input.clone()
    }
    /// Configure the existing controller-only default-on clipboard before
    /// service. No native open/read happens before bilateral readiness and the
    /// original control grant. Consent and the visible off switch stay separate.
    pub fn configure_clipboard(
        &mut self,
        configuration: frd::native_clipboard::Configuration,
    ) -> Result<frd::native_clipboard::Control, Error> {
        if self.state() != State::Viewing {
            return Err(Error::NotViewing);
        }
        self.observer
            .as_mut()
            .ok_or(Error::NotViewing)?
            .configure_clipboard(configuration)
            .map_err(Error::Clipboard)
    }
    /// Preserve the existing bounded terminal clipboard receipt after service or
    /// close. Neither this call nor cleanup creates another clipboard owner.
    pub fn collect_clipboard(&mut self) -> Result<(), Error> {
        self.observer
            .as_mut()
            .ok_or(Error::NotViewing)?
            .collect_clipboard()
            .map_err(Error::Clipboard)
    }
    /// Build the original startup operation NOW, not when first polled. Approval,
    /// display choice, asynchronous window creation and first decode share its
    /// existing absolute budget. No native window exists before approved choice.
    /// `clock = Some` requires an already control-capable offer; it does not ask
    /// for input or invent the independent visibility proof required for control.
    ///
    /// A rejected second open leaves this desktop untouched and drops only the
    /// newly supplied Viewer. A failed/unpolled attempt is terminal, never retryable.
    pub fn open<'a>(
        &'a mut self,
        viewer: Viewer,
        policy: ObserverPolicy,
        clock: Option<ClockPolicy>,
        mut choose: impl FnMut(&Catalog) -> Result<Option<u128>, ()> + 'a,
        approval: impl FnMut(fr_client::startup::ApprovalNotice) -> Result<(), ()> + 'a,
    ) -> Result<impl Future<Output = Result<(), Error>> + 'a, Error> {
        if self.state != State::New {
            return Err(Error::AlreadyUsed);
        }
        let stop = viewer.control();
        self.stop = Some(stop.clone());
        self.state = State::Opening;
        let configuration = &self.configuration;
        let window = &mut self.window;
        let picker = &mut self.picker;
        let renderer_started = &mut self.renderer_started;
        let retirement = &mut self.retirement;
        let picker_stop = stop.clone();
        let observer = &mut self.observer;
        // One content-free failure slot, shared only by these startup futures.
        // This is not a native callback queue and retains no pixels or input.
        let failure = Rc::new(Cell::new(None));
        let report = failure.clone();
        let selection_failure = failure.clone();
        let opening = viewer.observe_with_renderer(
            move |display, original| async move {
                // Native decoders cannot start before this one-use factory is
                // polled and returns a Launch. False is positive no-renderer
                // evidence, not an inference from a missing worker handle.
                *renderer_started = true;
                let result = async {
                    *window = Some(ViewerWindow::start(
                        &configuration.display,
                        display.pixel_width,
                        display.pixel_height,
                        original,
                    )?);
                    let window = window.as_mut().ok_or(viewer_window::Error::NotReady)?;
                    window.ready().await?;
                    let launch = window.decoder_launch(
                        &configuration.image,
                        configuration.xauthority.as_deref(),
                        configuration.worker_epoch,
                    )?;
                    let (launch, owner) = launch
                        .retain_cleanup()
                        .map_err(viewer_window::Error::Launch)?;
                    // Store custody BEFORE the Launch escapes into any nested
                    // startup future. Failure, timeout, panic and future-drop
                    // then retain the exact child in this original attempt.
                    *retirement = Some(owner);
                    Ok(launch)
                }
                .await;
                result.map_err(|error| report.set(Some(Error::Window(error))))
            },
            policy,
            clock,
            move |catalog| {
                if !configuration.display_picker {
                    return choose(catalog);
                }
                let result = (|| {
                    if picker.is_none() {
                        *picker = Some(DisplayPicker::start(
                            &configuration.display,
                            *catalog,
                            picker_stop.clone(),
                        )?);
                    }
                    picker
                        .as_mut()
                        .ok_or(display_picker::Error::NativeFailure)?
                        .poll(catalog)
                })();
                result.map_err(|error| selection_failure.set(Some(Error::Picker(error))))
            },
            approval,
        );
        Ok(Operation {
            state: &mut self.state,
            stop,
            success: State::Viewing,
            complete: false,
            inner: Box::pin(async move {
                let ready = opening
                    .await
                    .map_err(|error| failure.get().unwrap_or(Error::Observer(error)))?;
                *observer = Some(ready);
                Ok(())
            }),
        })
    }
    /// Continue observation through the original decoder and connection. The UI
    /// sees compositor-submission metadata, not fabricated visible-frame proof.
    /// The service operation is terminal on return, error, panic or abandonment.
    pub fn serve<'a>(
        &'a mut self,
        ui: impl FnMut(Option<Presentation>) -> Result<(), ()> + 'a,
    ) -> Result<impl Future<Output = Result<(), StreamingViewerError>> + 'a, Error> {
        if self.state() != State::Viewing {
            return Err(Error::NotViewing);
        }
        let observer = self.observer.as_mut().ok_or(Error::NotViewing)?;
        let stop = observer.control();
        let inner = observer.serve(ui);
        Ok(Operation {
            state: &mut self.state,
            stop,
            success: State::Stopped,
            complete: false,
            inner: Box::pin(inner),
        })
    }
    /// Keep watching until the UI explicitly calls `request_control` on the
    /// original Viewing state. The UI must supply real mapping/visibility
    /// evidence through that state's existing APIs; this method fabricates none.
    ///
    /// After the original grant, the UI may return ONE confirmed `Layout` to
    /// start X11 input capture. It must describe the exact native-pixel image in
    /// THIS desktop's window: cropping, scaling or another display is refused.
    /// Return None while viewing/requesting and after attachment. The capture
    /// thread remains owned by the original controlled session across every exit.
    /// Input results keep their admitted/submitted/observed distinctions.
    pub fn serve_interactive<'a>(
        &'a mut self,
        sequence: u64,
        capabilities: fr_core::input_submission::Capabilities,
        policy: fr_client::input::Policy,
        mut ui: impl FnMut(
            frd::session_startup::InteractiveViewerState<'_>,
            Option<Presentation>,
        ) -> Result<Option<frd::session_startup::viewer_events::Layout>, ()>
        + 'a,
        result: impl FnMut(fr_client::input::ResultEvent) + 'a,
    ) -> Result<impl Future<Output = Result<(), Error>> + 'a, Error> {
        if self.state() != State::Viewing {
            return Err(Error::NotViewing);
        }
        let window = self.window().ok_or(Error::NotViewing)?;
        let display = self.observer.as_ref().ok_or(Error::NotViewing)?.display();
        let local = &self.configuration.display;
        let input = &mut self.input;
        let observer = self.observer.as_mut().ok_or(Error::NotViewing)?;
        let stop = observer.control();
        let failure = Rc::new(Cell::new(None));
        let report = failure.clone();
        let inner = observer.serve_interactive_control(
            sequence,
            capabilities,
            policy,
            move |state, presentation| {
                use frd::session_startup::InteractiveViewerState;
                let attached = match state {
                    InteractiveViewerState::Controlled(viewer) => {
                        match ui(InteractiveViewerState::Controlled(viewer), presentation)? {
                            None => Ok(()),
                            Some(layout) => (|| {
                                if input.is_some() {
                                    return Err(Error::CaptureAlreadyStarted);
                                }
                                let target = window.input_window().map_err(Error::Window)?;
                                check_layout(display, target, &layout)?;
                                *input = Some(
                                    crate::viewer_input::X11InputCapture::attach(
                                        viewer, local, target, layout,
                                    )
                                    .map_err(Error::InputCapture)?,
                                );
                                Ok(())
                            })(),
                        }
                    }
                    other => {
                        if ui(other, presentation)?.is_some() {
                            Err(Error::NotViewing)
                        } else {
                            Ok(())
                        }
                    }
                };
                attached.map_err(|error| report.set(Some(error)))
            },
            result,
        );
        Ok(Operation {
            state: &mut self.state,
            stop,
            success: State::Stopped,
            complete: false,
            inner: Box::pin(async move {
                inner
                    .await
                    .map_err(|error| failure.get().unwrap_or(Error::Observer(error)))
            }),
        })
    }
    /// Fence the original session before stopping either native resource. Keep
    /// owners for reaping; this method never blocks or claims cleanup completed.
    pub fn close(&mut self) {
        if let Some(stop) = &self.stop {
            stop.stop();
        }
        if let Some(observer) = &mut self.observer {
            observer.close();
        }
        if let Some(window) = &self.window {
            window.control().stop();
        }
        self.state = State::Stopped;
    }
    pub fn window_cleanup(&mut self) -> WindowCleanup {
        self.window
            .as_mut()
            .map_or(WindowCleanup::NotStarted, |window| {
                window
                    .finish()
                    .map_or(WindowCleanup::Pending, WindowCleanup::Complete)
            })
    }
    /// Stop at CALL time, even when the returned cleanup future is never polled.
    /// Use an independent cleanup Cx and its original deadline. A pending/failed
    /// reap retains all owners and may be collected again without reconnecting.
    pub fn reap<'a>(
        &'a mut self,
        cleanup: &'a Cx,
        deadline: Deadline,
    ) -> impl Future<Output = Cleanup> + 'a {
        self.close();
        async move {
            let media = match &mut self.observer {
                Some(observer) => observer.reap_media(cleanup, deadline).await.map(Some),
                None => match &mut self.retirement {
                    Some(owner) => owner
                        .reap(cleanup, deadline)
                        .await
                        .map_err(frd::media::Error::Worker),
                    // The only Launch factory stores retirement before returning
                    // it. Absence here proves no launch escaped, even if a native
                    // window was opened and then failed before decoder startup.
                    None => Ok(None),
                },
            };
            let input = self.observer.as_mut().map_or(
                CaptureCleanup::NotStarted,
                NativeObserver::input_capture_cleanup,
            );
            let clipboard = match &mut self.observer {
                Some(observer) => observer.reap_clipboard(cleanup, deadline).await,
                None => Ok(frd::native_clipboard::Cleanup::NotStarted),
            };
            Cleanup {
                media,
                input,
                window: self.window_cleanup(),
                picker: self.picker_cleanup(),
                clipboard,
            }
        }
    }
}
impl fmt::Debug for Desktop {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NativeDesktop")
            .field("state", &self.state())
            .finish_non_exhaustive()
    }
}
impl Drop for Desktop {
    fn drop(&mut self) {
        self.close();
    }
}

// The native decoder writes the full selected image at (0,0). A generic
// Viewport can express crop/zoom/letterbox, but those do not describe THESE pixels.
fn check_layout(
    display: Display,
    window: crate::viewer_input::Window,
    layout: &frd::session_startup::viewer_events::Layout,
) -> Result<(), Error> {
    use fr_client::input::viewport::SurfaceRect;
    use fr_core::input::{DesktopPoint, InputBounds};
    let source = InputBounds::new(
        DesktopPoint {
            x: display.x,
            y: display.y,
        },
        display.pixel_width,
        display.pixel_height,
    )
    .ok_or(Error::LayoutMismatch)?;
    let destination =
        SurfaceRect::new(0, 0, window.width, window.height).map_err(|_| Error::LayoutMismatch)?;
    if window.width != display.pixel_width
        || window.height != display.pixel_height
        || layout.source() != source
        || layout.destination() != destination
    {
        return Err(Error::LayoutMismatch);
    }
    Ok(())
}

// Outside the async body so its destructor fences BEFORE dropping pending work.
// A borrow of the state prevents overlapping operations on the same Desktop.
struct Operation<'a, F> {
    state: &'a mut State,
    stop: StreamingViewerControl,
    success: State,
    complete: bool,
    inner: Pin<Box<F>>,
}
impl<T, E, F: Future<Output = Result<T, E>>> Future for Operation<'_, F> {
    type Output = F::Output;
    fn poll(self: Pin<&mut Self>, task: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let result = this.inner.as_mut().poll(task);
        if let Poll::Ready(value) = &result {
            this.complete = value.is_ok();
            *this.state = if this.complete {
                this.success
            } else {
                State::Stopped
            };
            if *this.state == State::Stopped {
                this.stop.stop();
            }
        }
        result
    }
}
impl<F> Drop for Operation<'_, F> {
    fn drop(&mut self) {
        if !self.complete {
            self.stop.stop();
            *self.state = State::Stopped;
        }
    }
}

#[cfg(test)]
mod tests;
