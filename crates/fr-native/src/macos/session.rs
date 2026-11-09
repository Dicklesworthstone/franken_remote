//! The macOS client session (fr-5a4w). frd's reconnecting supervisor calls
//! [`Session`] once per authenticated attempt; each attempt gets its own
//! window, opened only after the host's display was selected and sized to it,
//! and its own in-process `VideoToolbox` decoder drawing through Metal.
//!
//! With control, the session confirms the window's mapping, reports frames as
//! visible only while the window is actually on screen, requests control once
//! the session reports current evidence, and after the grant hands `AppKit`
//! input to the granted session's one event `Source`. Focus loss, hiding, a
//! changed picture size, event overflow or a closed window stop that source
//! (frd's contract for a native producer): control then ends with the session.
//!
//! Main thread only: `AppKit` owns the window and its event loop, which this
//! session services between network turns of the same runtime.
use super::{HevcDecoder, PicturePoint, Surface, Window, WindowEvent};
use crate::NativeError;
use asupersync::{
    cx::Cx,
    time::{Sleep, sleep},
};
use fr_client::input::viewport::SurfaceRect;
use fr_core::input::{
    DesktopPoint, InputBounds, KeyTransition, PhysicalKey, PointerButton, ScrollUnit,
};
use fr_core::input_submission::{Capabilities, Capability};
use fr_media::{
    access_unit::EncodedAccessUnit,
    freshness::ClockPolicy,
    hevc::DecoderRecord,
    worker::{
        Configuration as WorkerConfiguration, Error as WorkerError, overlay::Update as CursorUpdate,
    },
};
use fr_wire::display::{Catalog, Display};
use frd::{
    media::PresentationStage,
    native_connection::reconnect::{Application, CallbackError, Status},
    session_startup::{
        ControlledViewer, InteractiveViewerState, NativeObserver, ObserverError, ObserverPolicy,
        Presentation, StreamingViewerControl, Viewer,
        viewer_events::{Event, Layout, LocalPoint, NativeCapture, PositionedAction, Source},
    },
    worker::{Deadline, Launch, LocalPresenter},
};
use std::{
    cell::{Cell, RefCell},
    future::{Future, poll_fn},
    pin::{Pin, pin},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
    task::Poll,
    time::Duration,
};

/// How often `AppKit` is serviced while the network is idle.
const PUMP: Duration = Duration::from_millis(4);
/// A wheel notch is one line; precise (trackpad) deltas are pixels.
const PIXELS_PER_LINE: f64 = 12.0;
/// The protocol's 16.16 fixed-point line.
const LINE: i32 = 1 << 16;

/// The control request this session makes: the same grant profile as the
/// Linux viewer (keys, repeat, absolute pointer, buttons, line scroll).
#[derive(Debug, Clone, Copy)]
pub struct ControlPolicy {
    pub sequence: u64,
    pub capabilities: Capabilities,
    pub input: fr_client::input::Policy,
    pub clock: ClockPolicy,
}
impl Default for ControlPolicy {
    fn default() -> Self {
        Self {
            sequence: 1,
            capabilities: Capabilities::default()
                .with(Capability::Keys)
                .with(Capability::Repeat)
                .with(Capability::Absolute)
                .with(Capability::Buttons)
                .with(Capability::LineScroll),
            input: fr_client::input::Policy::default(),
            clock: ClockPolicy::default(),
        }
    }
}

/// Why local control (or the whole session) ended on this side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalStop {
    WindowClosed,
    FocusLost,
    WindowHidden,
    PictureChanged,
    Overflow,
    NativeFailure,
}

/// Content-free tallies for the CLI's completion line.
#[derive(Debug, Default, Clone, Copy)]
pub struct Report {
    pub attempts: u8,
    pub opened: u32,
    pub presented: u64,
    pub hardware_decode: Option<bool>,
    pub approval_requested: bool,
    pub control_requested: bool,
    pub control_granted: bool,
    pub keys: u64,
    pub pointer: u64,
    pub buttons: u64,
    pub scrolls: u64,
    pub input_results: u64,
    pub local_stop: Option<LocalStop>,
}

type Choose = Box<dyn FnMut(&Catalog) -> Result<Option<u128>, ()>>;

/// The one producer frd owns for a granted session. `stop` is lock-free;
/// `try_reap` reports done once the event source has been dropped.
struct Capture {
    stop: Arc<AtomicBool>,
    released: Arc<AtomicBool>,
}
impl NativeCapture for Capture {
    fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
    fn try_reap(&mut self) -> bool {
        self.released.load(Ordering::SeqCst)
    }
}
impl Drop for Capture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// The granted session's event source and the layout it was attached with.
struct Input {
    source: Source,
    layout: Layout,
    picture: (u32, u32),
    stop: Arc<AtomicBool>,
    released: Arc<AtomicBool>,
    keys: [bool; 256],
    buttons: [bool; 5],
    scroll: (f64, f64),
}
impl Drop for Input {
    fn drop(&mut self) {
        self.source.stop();
        self.released.store(true, Ordering::SeqCst);
    }
}
impl Input {
    fn local(&self, at: PicturePoint) -> LocalPoint {
        // Subpixels of the picture as laid out: 256 per drawable pixel.
        let scale = |v: f64, n: u32| {
            let v = if v.is_finite() {
                v.clamp(0.0, 1.0)
            } else {
                0.0
            };
            // In range by the clamp: at most n * 256.
            #[allow(clippy::cast_possible_truncation)]
            let p = (v * f64::from(n) * 256.0) as i64;
            p.min(i64::from(n) * 256 - 1)
        };
        LocalPoint::subpixels(scale(at.x, self.picture.0), scale(at.y, self.picture.1))
    }
    /// One window event to the granted session. Err means stop control.
    fn deliver(&mut self, event: &WindowEvent, report: &mut Report) -> Result<(), ()> {
        let caps = self.source.capabilities();
        let sampled = self.source.clock().map_err(|_| ())?;
        let event = match *event {
            WindowEvent::Key {
                usage,
                pressed,
                repeat,
            } => {
                let Some(key) = PhysicalKey::new(usage) else {
                    return Ok(());
                };
                let held = &mut self.keys[usize::from(usage & 0xff)];
                let transition = match (pressed, *held, repeat) {
                    (true, false, _) => KeyTransition::Press,
                    (true, true, true) if caps.contains(Capability::Repeat) => {
                        KeyTransition::Repeat
                    }
                    (false, true, _) => KeyTransition::Release,
                    // A release of a key pressed before the grant, or a
                    // repeat without the capability: nothing to send.
                    _ => return Ok(()),
                };
                *held = pressed;
                report.keys += 1;
                Event::Key { key, transition }
            }
            WindowEvent::Pointer(at) => {
                if !at.inside || !caps.contains(Capability::Absolute) {
                    return Ok(());
                }
                report.pointer += 1;
                Event::Pointer(self.layout.at(self.local(at)))
            }
            WindowEvent::Button {
                button,
                pressed,
                at,
            } => {
                let (index, button) = match button {
                    0 => (0, PointerButton::Primary),
                    1 => (1, PointerButton::Secondary),
                    2 => (2, PointerButton::Middle),
                    3 => (3, PointerButton::Back),
                    4 => (4, PointerButton::Forward),
                    _ => return Ok(()),
                };
                if !caps.contains(Capability::Buttons) || self.buttons[index] == pressed {
                    return Ok(());
                }
                self.buttons[index] = pressed;
                report.buttons += 1;
                Event::Positioned {
                    location: self.layout.at(self.local(at)),
                    action: PositionedAction::Button { button, pressed },
                }
            }
            WindowEvent::Scroll {
                dx,
                dy,
                precise,
                at,
            } => {
                if !caps.contains(Capability::LineScroll) || !at.inside {
                    return Ok(());
                }
                let per_line = if precise { PIXELS_PER_LINE } else { 1.0 };
                self.scroll.0 += dx / per_line;
                self.scroll.1 += dy / per_line;
                // Whole lines only, like a wheel notch. AppKit's positive
                // delta scrolls up (or left): the protocol's negative.
                let whole = |acc: &mut f64| {
                    let lines = acc.trunc();
                    *acc -= lines;
                    // Bounded by the per-event delta AppKit reports.
                    #[allow(clippy::cast_possible_truncation)]
                    let lines = lines.clamp(-64.0, 64.0) as i32;
                    -lines * LINE
                };
                let (x, y) = (whole(&mut self.scroll.0), whole(&mut self.scroll.1));
                if x == 0 && y == 0 {
                    return Ok(());
                }
                report.scrolls += 1;
                Event::Positioned {
                    location: self.layout.at(self.local(at)),
                    action: PositionedAction::Scroll {
                        x,
                        y,
                        unit: ScrollUnit::Lines,
                    },
                }
            }
            _ => return Ok(()),
        };
        self.source.push(event, sampled).map_err(|_| ())
    }
}

/// The in-process presenter frd's Worker drives: the window's Metal surface
/// until configured, then the admitted `VideoToolbox` decoder drawing into it.
struct Presenter {
    surface: Option<Surface>,
    decoder: Option<HevcDecoder>,
    hardware: Hardware,
}
/// Whether `VideoToolbox` chose a hardware decoder, as it last said; shared
/// with the presenter frd's `Worker` owns (which must be `Send`).
#[derive(Clone, Default)]
struct Hardware(Arc<AtomicU8>);
impl Hardware {
    fn set(&self, hardware: Option<bool>) {
        let code = match hardware {
            None => 0,
            Some(false) => 1,
            Some(true) => 2,
        };
        self.0.store(code, Ordering::Relaxed);
    }
    fn get(&self) -> Option<bool> {
        match self.0.load(Ordering::Relaxed) {
            1 => Some(false),
            2 => Some(true),
            _ => None,
        }
    }
}
/// The window's starting size in points: the remote display's logical size,
/// so a 2x remote lands 1:1 on a 2x screen and a 1x remote keeps its natural
/// size. A logical size whose aspect ratio is off the picture's by more than a
/// pixel falls back to the pixel size.
fn natural_points(display: &Display) -> (u32, u32) {
    let (pw, ph) = (
        u64::from(display.pixel_width),
        u64::from(display.pixel_height),
    );
    let (lw, lh) = (
        u64::from(display.logical_width),
        u64::from(display.logical_height),
    );
    if lw == 0 || lh == 0 || (lw * ph).abs_diff(lh * pw) > pw.max(ph) {
        (display.pixel_width, display.pixel_height)
    } else {
        (display.logical_width, display.logical_height)
    }
}
fn worker_error(error: NativeError) -> WorkerError {
    match error {
        NativeError::InvalidConfiguration | NativeError::UnsupportedBitstream => {
            WorkerError::Unsupported
        }
        NativeError::StaleGeneration => WorkerError::WrongEpoch,
        NativeError::Allocation => WorkerError::Allocation,
        NativeError::GeometryChanged => WorkerError::GeometryChanged,
        _ => WorkerError::NativeFailure,
    }
}
impl LocalPresenter for Presenter {
    fn configure(
        &mut self,
        configuration: &WorkerConfiguration,
        record: &DecoderRecord,
    ) -> Result<(), WorkerError> {
        if record.generation() != configuration.generation {
            return Err(WorkerError::WrongEpoch);
        }
        let surface = self.surface.take().ok_or(WorkerError::WrongState)?;
        let decoder = HevcDecoder::new(
            surface,
            configuration.codec()?,
            configuration.limits()?,
            record.bytes(),
        )
        .map_err(worker_error)?;
        self.hardware.set(decoder.hardware());
        self.decoder = Some(decoder);
        Ok(())
    }
    fn submit(&mut self, unit: &EncodedAccessUnit, present: bool) -> Result<(), WorkerError> {
        self.decoder
            .as_mut()
            .ok_or(WorkerError::WrongState)?
            .submit_unit(unit, present)
            .map_err(worker_error)
    }
    fn overlay(&mut self, update: &CursorUpdate<'_>) -> Result<(), WorkerError> {
        let surface = match (&self.decoder, &self.surface) {
            (Some(decoder), _) => decoder.surface(),
            (None, Some(surface)) => surface,
            (None, None) => return Err(WorkerError::WrongState),
        };
        surface.cursor(update).map_err(worker_error)
    }
}

/// This attempt's window, kept until mandatory cleanup. The pump and the
/// UI callbacks take only short borrows of it.
type WindowSlot = Rc<RefCell<Option<Window>>>;

pub struct Session {
    control: Option<ControlPolicy>,
    policy: ObserverPolicy,
    choose: Choose,
    epoch: u128,
    title: String,
    window: WindowSlot,
    /// The started observer, reaped in `cleanup`. `run` and `cleanup` both
    /// take `&mut self`, so `run`'s borrow has ended before cleanup starts.
    observer: Option<NativeObserver>,
    input: Rc<RefCell<Option<Input>>>,
    report: Rc<RefCell<Report>>,
    closed: Rc<Cell<bool>>,
}
impl Session {
    /// `control` None observes; Some requests control once per attempt.
    /// `choose` picks the host display (Ok(None) is not supported: the CLI
    /// chooses from its arguments). `epoch` must be nonzero.
    pub fn new(
        control: Option<ControlPolicy>,
        policy: ObserverPolicy,
        choose: Choose,
        epoch: u128,
        title: String,
    ) -> Self {
        Self {
            control,
            policy,
            choose,
            epoch,
            title,
            window: Rc::default(),
            observer: None,
            input: Rc::default(),
            report: Rc::default(),
            closed: Rc::default(),
        }
    }
    pub fn report(&self) -> Report {
        *self.report.borrow()
    }
    /// The user closed the window: no further attempt should start.
    pub fn user_closed(&self) -> bool {
        self.closed.get()
    }
}

/// Service `AppKit` between network turns: `turn` runs whenever `work` is
/// polled and at least every `PUMP` while it is idle.
async fn pumped<T>(work: impl Future<Output = T>, mut turn: impl FnMut()) -> T {
    let mut work = pin!(work);
    let timer = Cx::current().and_then(|cx| cx.timer_driver());
    let mut tick: Option<Pin<Box<Sleep>>> = None;
    poll_fn(move |task| {
        loop {
            if let Poll::Ready(out) = work.as_mut().poll(task) {
                return Poll::Ready(out);
            }
            turn();
            let Some(timer) = &timer else {
                return Poll::Pending;
            };
            let next = tick.get_or_insert_with(|| Box::pin(sleep(timer.now(), PUMP)));
            if next.as_mut().poll(task).is_pending() {
                return Poll::Pending;
            }
            tick = None;
        }
    })
    .await
}

/// Drain the window's events, deliver them to a granted session, and stop
/// control (or the session) on the local conditions frd requires.
fn service(
    window: &RefCell<Option<Window>>,
    input: &RefCell<Option<Input>>,
    report: &RefCell<Report>,
    closed: &Cell<bool>,
    stop: &StreamingViewerControl,
    events: &mut Vec<WindowEvent>,
) {
    let mut slot = window.borrow_mut();
    let Some(window) = slot.as_mut() else {
        return;
    };
    events.clear();
    window.poll(Duration::ZERO, events);
    let state = window.state().ok();
    drop(slot);
    let mut report = report.borrow_mut();
    let mut input = input.borrow_mut();
    let halt = |reason: LocalStop, input: &mut Option<Input>, report: &mut Report| {
        report.local_stop.get_or_insert(reason);
        // Dropping the source fences the granted session.
        *input = None;
    };
    for event in events.iter() {
        match event {
            WindowEvent::CloseRequested => {
                closed.set(true);
                halt(LocalStop::WindowClosed, &mut input, &mut report);
                stop.stop();
            }
            WindowEvent::FocusLost if input.is_some() => {
                halt(LocalStop::FocusLost, &mut input, &mut report);
            }
            WindowEvent::Overflow if input.is_some() => {
                halt(LocalStop::Overflow, &mut input, &mut report);
            }
            other => {
                if let Some(granted) = input.as_mut()
                    && granted.deliver(other, &mut report).is_err()
                {
                    halt(LocalStop::NativeFailure, &mut input, &mut report);
                }
            }
        }
    }
    if let Some(granted) = input.as_ref() {
        let reason = if granted.stop.load(Ordering::SeqCst) {
            Some(LocalStop::NativeFailure)
        } else {
            match state {
                None => Some(LocalStop::NativeFailure),
                Some(s) if !s.key => Some(LocalStop::FocusLost),
                Some(s) if !s.visible => Some(LocalStop::WindowHidden),
                Some(s) if s.picture != granted.picture => Some(LocalStop::PictureChanged),
                Some(_) => None,
            }
        };
        if let Some(reason) = reason {
            // frd's own stop is not a local failure: record only ours.
            if !granted.stop.load(Ordering::SeqCst) {
                report.local_stop.get_or_insert(reason);
            }
            *input = None;
        }
    }
}

/// The frame's on-screen evidence: drawn through Metal while some of the
/// window is visible. Decode-only frames are never visible.
fn shown(window: &RefCell<Option<Window>>, frame: Option<Presentation>) -> Option<u64> {
    let frame = frame?;
    if frame.stage != PresentationStage::SubmittedToCompositor {
        return None;
    }
    let visible = window
        .borrow()
        .as_ref()
        .and_then(|w| w.state().ok())
        .is_some_and(|s| s.visible);
    visible.then_some(frame.frame.as_raw())
}

/// Attach this window's input to the granted session with a layout that
/// maps the whole picture, as drawn now, onto the selected display.
fn attach(
    viewer: &mut ControlledViewer,
    display: Display,
    window: &RefCell<Option<Window>>,
    slot: &Rc<RefCell<Option<Input>>>,
) -> Result<bool, ()> {
    let window = window.borrow();
    let window = window.as_ref().ok_or(())?;
    let state = window.state().map_err(|_| ())?;
    let (width, height) = state.picture;
    if width == 0 || height == 0 || !state.key || !state.visible {
        return Ok(false); // wait for a laid-out, focused, visible window
    }
    let source = InputBounds::new(
        DesktopPoint {
            x: display.x,
            y: display.y,
        },
        display.pixel_width,
        display.pixel_height,
    )
    .ok_or(())?;
    let area = SurfaceRect::new(0, 0, width, height).map_err(|_| ())?;
    let layout = viewer.configure_viewport(source, area).map_err(|_| ())?;
    viewer.confirm_viewport(&layout).map_err(|_| ())?;
    // The layout is valid for this size only.
    window.set_resizable(false).map_err(|_| ())?;
    let stop = Arc::new(AtomicBool::new(false));
    let released = Arc::new(AtomicBool::new(false));
    let capture = Capture {
        stop: stop.clone(),
        released: released.clone(),
    };
    viewer
        .capture_input_owned(&layout.clone(), |source| {
            *slot.borrow_mut() = Some(Input {
                source,
                layout,
                picture: (width, height),
                stop,
                released,
                keys: [false; 256],
                buttons: [false; 5],
                scroll: (0.0, 0.0),
            });
            Ok::<_, ()>(capture)
        })
        .map_err(|_| ())?;
    Ok(true)
}

impl Application for Session {
    type Output = ();
    #[allow(clippy::too_many_lines)] // one attempt: open, serve, control
    async fn run(&mut self, attempt: u8, viewer: Viewer) -> Result<(), ObserverError> {
        self.report.borrow_mut().attempts = attempt;
        let stop = viewer.control();
        let window = self.window.clone();
        let input = self.input.clone();
        let report = self.report.clone();
        let closed = self.closed.clone();
        let hardware = Hardware::default();
        let epoch = self.epoch.wrapping_add(u128::from(attempt)).max(1);
        let title = self.title.clone();
        let control = self.control;
        let choose = &mut self.choose;
        let window_slot = window.clone();
        let decoder_hardware = hardware.clone();
        let approval_report = report.clone();
        let opening = viewer.observe_with_renderer(
            move |display, _original| async move {
                let opened = Window::open(
                    display.pixel_width,
                    display.pixel_height,
                    natural_points(&display),
                    &title,
                )
                .map_err(|_| ())?;
                let surface = opened.surface();
                *window_slot.borrow_mut() = Some(opened);
                Launch::in_process(
                    Box::new(Presenter {
                        surface: Some(surface),
                        decoder: None,
                        hardware: decoder_hardware,
                    }),
                    epoch,
                )
                .map_err(|_| ())
            },
            self.policy,
            control.map(|c| c.clock),
            move |catalog| choose(catalog),
            move |_notice| {
                approval_report.borrow_mut().approval_requested = true;
                Ok(())
            },
        );
        let mut events = Vec::with_capacity(64);
        let mut pump = || service(&window, &input, &report, &closed, &stop, &mut events);
        let opened = pumped(opening, &mut pump).await?;
        {
            let mut r = report.borrow_mut();
            r.opened += 1;
            r.hardware_decode = hardware.get();
        }
        let display = opened.display();
        let observer = self.observer.insert(opened);
        let count = report.clone();
        let frames = move |frame: Option<Presentation>| {
            if frame.is_some() {
                count.borrow_mut().presented += 1;
            }
        };
        match control {
            None => {
                let closing = closed.clone();
                let served = observer.serve_until(move |frame| {
                    frames(frame);
                    Ok(if closing.get() {
                        std::ops::ControlFlow::Break(fr_wire::closure::Reason::Requested)
                    } else {
                        std::ops::ControlFlow::Continue(())
                    })
                });
                pumped(served, &mut pump)
                    .await
                    .map(|_| ())
                    .map_err(ObserverError::Streaming)
            }
            Some(policy) => {
                let mut mapped = false;
                let mut last_shown = None;
                let mut requested = false;
                let mut attached = false;
                let ui_window = window.clone();
                let ui_input = input.clone();
                let ui_report = report.clone();
                let results = report.clone();
                let served = observer.serve_interactive_control(
                    policy.sequence,
                    policy.capabilities,
                    policy.input,
                    move |state, frame| {
                        frames(frame);
                        let fresh = shown(&ui_window, frame).filter(|f| last_shown != Some(*f));
                        match state {
                            InteractiveViewerState::Viewing(view) => {
                                let Some(frame) = fresh.or(last_shown) else {
                                    return Ok(());
                                };
                                if !mapped {
                                    let request = view.request();
                                    view.confirm_mapping(request.parent, request.target.view)
                                        .map_err(|_| ())?;
                                    mapped = true;
                                }
                                if fresh.is_some() && view.visible(frame).is_ok() {
                                    last_shown = Some(frame);
                                }
                                if !requested && view.evidence().is_ok() {
                                    requested = true;
                                    ui_report.borrow_mut().control_requested = true;
                                    view.request_control().map_err(|_| ())?;
                                }
                            }
                            InteractiveViewerState::Requesting(pending) => {
                                if let Some(frame) = fresh
                                    && pending.visible(frame).is_ok()
                                {
                                    last_shown = Some(frame);
                                }
                            }
                            InteractiveViewerState::Controlled(viewer) => {
                                ui_report.borrow_mut().control_granted = true;
                                if let Some(frame) = fresh {
                                    let _ = viewer.visible(frame);
                                    last_shown = Some(frame);
                                }
                                if !attached {
                                    attached = attach(viewer, display, &ui_window, &ui_input)?;
                                }
                            }
                        }
                        Ok(())
                    },
                    move |_result| results.borrow_mut().input_results += 1,
                );
                // Already the observer's own error type.
                pumped(served, &mut pump).await
            }
        }
    }
    fn cleanup(
        &mut self,
        cx: &Cx,
        deadline: Deadline,
    ) -> impl Future<Output = Result<(), CallbackError>> {
        // Fence at call time: no input outlives the attempt.
        self.input.borrow_mut().take();
        let observer = self.observer.take();
        let window = self.window.clone();
        async move {
            let result = match observer {
                Some(mut observer) => {
                    let media = observer.reap_media(cx, deadline).await;
                    let input = observer.input_capture_cleanup();
                    media.is_ok()
                        && input != frd::session_startup::viewer_events::CaptureCleanup::Pending
                }
                // Startup failed before an observer existed: the presenter
                // (and its decoder) went with frd's dropped Worker.
                None => true,
            };
            window.borrow_mut().take();
            if result { Ok(()) } else { Err(CallbackError) }
        }
    }
    fn status(&mut self, status: Status) -> Result<(), CallbackError> {
        match status {
            Status::Reconnecting { .. } | Status::Reopening { .. }
                if self.closed.get() || self.report.borrow().local_stop.is_some() =>
            {
                Err(CallbackError)
            }
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::natural_points;
    use fr_core::ids::DisplayGeometryGeneration;
    use fr_wire::display::Display;

    fn display(pixels: (u32, u32), logical: (u32, u32)) -> Display {
        Display {
            handle: 1,
            geometry: DisplayGeometryGeneration::INITIAL,
            x: 0,
            y: 0,
            pixel_width: pixels.0,
            pixel_height: pixels.1,
            logical_width: logical.0,
            logical_height: logical.1,
            scale_numerator: 1,
            scale_denominator: 1,
            rotation: 0,
        }
    }

    #[test]
    fn windows_open_at_the_remote_logical_size() {
        // A Retina Mac or a 200% Windows display: half the pixels in points.
        assert_eq!(
            natural_points(&display((5120, 2880), (2560, 1440))),
            (2560, 1440)
        );
        assert_eq!(
            natural_points(&display((3000, 2000), (1500, 1000))),
            (1500, 1000)
        );
        // A 1x X11 display is unchanged; rounding within a pixel is accepted.
        assert_eq!(
            natural_points(&display((1920, 1080), (1920, 1080))),
            (1920, 1080)
        );
        assert_eq!(
            natural_points(&display((2880, 1800), (1919, 1200))),
            (1919, 1200)
        );
        // A logical size of another shape falls back to the picture's pixels.
        assert_eq!(
            natural_points(&display((1920, 1080), (1080, 1920))),
            (1920, 1080)
        );
        assert_eq!(natural_points(&display((1920, 1080), (0, 0))), (1920, 1080));
    }
}
