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
//! The invocation's initial control choice is consumed once. A recovered
//! window stays viewing until a fresh F8 press and release requests a new
//! grant; Ctrl+Alt+Shift+Escape closes the session locally.
//!
//! Main thread only: `AppKit` owns the window and its event loop, which this
//! session services between network turns of the same runtime.
use super::{HevcDecoder, PicturePoint, Surface, Window, WindowEvent, event_time};
use crate::NativeError;
use crate::macos_control::{Intent, Key, Route};
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
    LocalEscape,
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
    pub control_requests: u32,
    pub viewing_reconnects: u32,
    pub keys: u64,
    pub pointer: u64,
    pub buttons: u64,
    pub scrolls: u64,
    pub input_results: u64,
    pub local_stop: Option<LocalStop>,
    /// Actual host exchange stages; absence never confirms remote cleanup.
    pub close_exchange: Option<fr_transport::quic::CloseOutcome>,
    pub lease_revocation: Option<fr_wire::lease_revoked::Revoked>,
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
                ..
            } => {
                let Some(key) = PhysicalKey::new(usage) else {
                    return Ok(());
                };
                let held = &mut self.keys[usize::from(usage & 0xff)];
                let transition = match (pressed, *held, repeat) {
                    (true, false, false) => KeyTransition::Press,
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
    fn refresh(&mut self) -> Result<(), WorkerError> {
        self.decoder
            .as_ref()
            .ok_or(WorkerError::WrongState)?
            .surface()
            .refresh()
            .map_err(worker_error)
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
    fit_window: Option<(u32, u32)>,
    window: WindowSlot,
    /// The started observer, reaped in `cleanup`. `run` and `cleanup` both
    /// take `&mut self`, so `run`'s borrow has ended before cleanup starts.
    observer: Option<NativeObserver>,
    input: Rc<RefCell<Option<Input>>>,
    report: Rc<RefCell<Report>>,
    closed: Rc<Cell<bool>>,
    intent: Rc<RefCell<Intent>>,
    started: bool,
    /// Set inside the deferred renderer, after approval and display selection.
    /// Entering `run` alone does not mean any native cleanup owner exists.
    native_started: Rc<Cell<bool>>,
    cleanup_confirmed: bool,
    terminal: Option<ObserverError>,
}
impl Session {
    /// `control` None observes; Some consumes the invocation's initial control
    /// choice once. Recovered viewing requires a fresh local F8 choice.
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
            fit_window: None,
            window: Rc::default(),
            observer: None,
            input: Rc::default(),
            report: Rc::default(),
            closed: Rc::default(),
            intent: Rc::new(RefCell::new(Intent::new(control.is_some()))),
            started: false,
            native_started: Rc::default(),
            cleanup_confirmed: false,
            terminal: None,
        }
    }
    /// An optional logical point bound for the fitted viewer content.
    pub fn with_fit_window(mut self, fit: Option<(u32, u32)>) -> Self {
        self.fit_window = fit;
        self
    }
    pub fn report(&self) -> Report {
        *self.report.borrow()
    }
    /// The user closed the window: no further attempt should start.
    pub fn user_closed(&self) -> bool {
        self.closed.get()
    }
    /// True only after this attempt's original native owners were reaped.
    /// A local window-close request never substitutes for that evidence.
    pub fn cleanup_confirmed(&self) -> bool {
        self.cleanup_confirmed
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
            // A ready network/media future must not outrun a queued local
            // close, escape, focus change or geometry fence on this turn.
            turn();
            if let Poll::Ready(out) = work.as_mut().poll(task) {
                return Poll::Ready(out);
            }
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
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn service(
    window: &RefCell<Option<Window>>,
    input: &RefCell<Option<Input>>,
    report: &RefCell<Report>,
    intent: &RefCell<Intent>,
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
    let mut intent = intent.borrow_mut();
    let halt = |reason: LocalStop, input: &mut Option<Input>, report: &mut Report| {
        report.local_stop.get_or_insert(reason);
        // Dropping the source fences the granted session before another turn.
        *input = None;
        stop.stop();
    };
    // Validate the whole native batch and current window before forwarding any
    // action. A late FocusLost or overflow cannot follow earlier exported keys.
    if events.contains(&WindowEvent::CloseRequested) {
        closed.set(true);
        intent.invalidate();
        halt(LocalStop::WindowClosed, &mut input, &mut report);
        return;
    }
    // Resizing or a stuck media call must never suppress local escape. It
    // revokes work, so it needs no valid viewport, clock or remote evidence.
    if events.iter().any(|event| {
        matches!(*event, WindowEvent::Key { usage, pressed, modifiers, .. }
            if crate::macos_control::emergency_escape(usage, pressed, modifiers))
    }) {
        closed.set(true);
        intent.invalidate();
        halt(LocalStop::LocalEscape, &mut input, &mut report);
        return;
    }
    if input
        .as_ref()
        .is_some_and(|input| input.stop.load(Ordering::SeqCst))
    {
        intent.invalidate();
        *input = None;
        return;
    }
    let reason = if events.contains(&WindowEvent::FocusLost) {
        Some(LocalStop::FocusLost)
    } else if events.contains(&WindowEvent::Overflow) {
        Some(LocalStop::Overflow)
    } else {
        match state {
            None => Some(LocalStop::NativeFailure),
            Some(s) if !s.visible => Some(LocalStop::WindowHidden),
            Some(s) if !s.key => Some(LocalStop::FocusLost),
            Some(s) if !s.geometry_current => Some(LocalStop::PictureChanged),
            Some(s)
                if input
                    .as_ref()
                    .is_some_and(|input| input.picture != s.picture) =>
            {
                Some(LocalStop::PictureChanged)
            }
            Some(_) => None,
        }
    };
    if let Some(reason) = reason {
        intent.suspend();
        if input.is_some() || intent.requested() {
            halt(reason, &mut input, &mut report);
        }
        return;
    }
    let now = event_time();
    let mut routes = Vec::with_capacity(events.len());
    for event in events.iter() {
        let route = match *event {
            WindowEvent::Key {
                usage,
                pressed,
                repeat,
                captured,
                modifiers,
            } => intent.key(
                Key {
                    usage,
                    pressed,
                    repeat,
                    captured,
                    modifiers,
                },
                now,
            ),
            WindowEvent::Button {
                button, pressed, ..
            } => {
                intent.button(button, pressed);
                Route::Remote
            }
            WindowEvent::UnmappedKey { .. } => {
                intent.invalidate();
                Route::Remote
            }
            _ => Route::Remote,
        };
        if route == Route::Escape {
            closed.set(true);
            intent.invalidate();
            halt(LocalStop::LocalEscape, &mut input, &mut report);
            return;
        }
        routes.push(route);
    }
    for (event, route) in events.iter().zip(routes) {
        if route == Route::Remote
            && let Some(granted) = input.as_mut()
            && granted.deliver(event, &mut report).is_err()
        {
            intent.invalidate();
            halt(LocalStop::NativeFailure, &mut input, &mut report);
            return;
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
        .is_some_and(|s| s.visible && s.geometry_current);
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
    if !state.input_ready() {
        return Ok(false); // wait for a submitted, current, focused picture
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
        if self.started || self.native_started.get() || self.observer.is_some() {
            return Err(ObserverError::Order);
        }
        self.started = true;
        self.cleanup_confirmed = false;
        self.report.borrow_mut().attempts = attempt;
        let stop = viewer.control();
        let window = self.window.clone();
        let input = self.input.clone();
        let report = self.report.clone();
        let closed = self.closed.clone();
        let intent = self.intent.clone();
        let hardware = Hardware::default();
        let epoch = self.epoch.wrapping_add(u128::from(attempt)).max(1);
        let title = self.title.clone();
        let fit_window = self.fit_window;
        let control = self.control;
        let choose = &mut self.choose;
        let window_slot = window.clone();
        let decoder_hardware = hardware.clone();
        let approval_report = report.clone();
        let native_started = self.native_started.clone();
        let opening = viewer.observe_with_renderer(
            move |display, _original| async move {
                // From this point onward, losing the observer is not proof
                // that AppKit, the renderer or its worker finished cleanup.
                native_started.set(true);
                let opened = Window::open(
                    display.pixel_width,
                    display.pixel_height,
                    crate::macos_sizing::points(
                        (display.pixel_width, display.pixel_height),
                        (display.logical_width, display.logical_height),
                        fit_window,
                    )?,
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
        let mut pump = || {
            service(
                &window,
                &input,
                &report,
                &intent,
                &closed,
                &stop,
                &mut events,
            )
        };
        let opened = pumped(opening, &mut pump).await?;
        {
            let mut r = report.borrow_mut();
            r.opened += 1;
            r.hardware_decode = hardware.get();
            if control.is_some() && intent.borrow().needs_explicit_request() {
                r.viewing_reconnects += 1;
            }
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
                let mut attached = false;
                let ui_window = window.clone();
                let ui_input = input.clone();
                let ui_report = report.clone();
                let ui_intent = intent.clone();
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
                                    ui_intent.borrow_mut().take_request(false, event_time());
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
                                let local_ready = ui_window
                                    .borrow()
                                    .as_ref()
                                    .and_then(|window| window.state().ok())
                                    .is_some_and(super::WindowState::input_ready);
                                if ui_intent.borrow_mut().take_request(
                                    local_ready && view.evidence().is_ok(),
                                    event_time(),
                                ) {
                                    let mut report = ui_report.borrow_mut();
                                    report.control_requested = true;
                                    report.control_requests += 1;
                                    drop(report);
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
                                if !ui_intent.borrow().requested() {
                                    return Err(()); // no local request on this fresh owner
                                }
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
        self.intent.borrow_mut().invalidate();
        self.cleanup_confirmed = false;
        let window = self.window.clone();
        async move {
            let result = match self.observer.as_mut() {
                Some(observer) => {
                    let control = observer.control_disconnect_outcome();
                    {
                        let mut report = self.report.borrow_mut();
                        report.close_exchange = observer
                            .disconnect_outcome()
                            .or_else(|| control.map(|outcome| outcome.exchange));
                        report.lease_revocation = control.and_then(|outcome| outcome.revocation);
                    }
                    self.terminal = self.terminal.or_else(|| reported_terminal(observer));
                    observer.close();
                    let media = observer.reap_media(cx, deadline).await;
                    let input = observer.reap_input_capture(cx, deadline).await;
                    media.is_ok() && input.is_ok()
                }
                // Approval and display selection precede the deferred native
                // factory. Their failures have no native owners to reap. Once
                // that factory starts, dropping its future proves no cleanup.
                None => !self.native_started.get(),
            };
            if !result {
                return Err(CallbackError);
            }
            self.observer = None;
            window.borrow_mut().take();
            self.started = false;
            self.native_started.set(false);
            self.cleanup_confirmed = true;
            Ok(())
        }
    }
    fn status(&mut self, status: Status) -> Result<(), CallbackError> {
        match status {
            Status::Connecting { .. } => {
                if self.started || self.native_started.get() || self.observer.is_some() {
                    return Err(CallbackError);
                }
                self.native_started.set(false);
                self.cleanup_confirmed = false;
                self.terminal = None;
                self.report.borrow_mut().close_exchange = None;
                self.report.borrow_mut().lease_revocation = None;
                self.intent.borrow_mut().begin_attempt();
                Ok(())
            }
            Status::Reconnecting { .. } | Status::Reopening { .. }
                if self.closed.get() || self.report.borrow().local_stop.is_some() =>
            {
                Err(CallbackError)
            }
            _ => Ok(()),
        }
    }
    fn terminal_failure(&self) -> Option<ObserverError> {
        self.terminal
    }
}

/// Retain the original owner's authenticated result before its cleanup can
/// remove it. Local close, media exit and source release create no host proof.
fn reported_terminal(observer: &NativeObserver) -> Option<ObserverError> {
    use frd::session_startup::{ControlledViewerError, Error, StreamingViewerError};
    let viewing = observer.disconnect_outcome();
    let control = observer.control_disconnect_outcome();
    let revocation = control.and_then(|outcome| outcome.revocation);
    let closed = viewing
        .and_then(|outcome| outcome.report)
        .or_else(|| control.and_then(|outcome| outcome.exchange.report));
    let lease_error = |report| {
        ObserverError::Streaming(StreamingViewerError::Control(
            ControlledViewerError::LeaseRevoked(report),
        ))
    };
    let session_error = |report| {
        ObserverError::Streaming(StreamingViewerError::Session(Error::RemoteClosed(report)))
    };
    if let Some(report) = revocation
        && (report.reason != fr_wire::lease_revoked::Reason::ClientRequested
            || report.cleanup == fr_wire::lease_revoked::CleanupStage::Failed)
    {
        return Some(lease_error(report));
    }
    // A normal lease release must not hide a racing adverse session report.
    if let Some(report) = closed
        && (report.reason != fr_wire::closure::ClosedReason::ClientRequested
            || report.cleanup == fr_wire::closure::Cleanup::Incomplete)
    {
        return Some(session_error(report));
    }
    revocation
        .map(lease_error)
        .or_else(|| closed.map(session_error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::{runtime::RuntimeBuilder, types::Budget};

    #[test]
    fn local_fences_run_before_even_immediately_ready_session_work() {
        let runtime = RuntimeBuilder::current_thread().build().unwrap();
        let fenced = Cell::new(false);
        runtime.block_on(pumped(
            async {
                assert!(fenced.get(), "ready work must observe the local fence");
            },
            || fenced.set(true),
        ));
    }

    fn failed_startup(native_started: bool) -> Session {
        let mut session = Session::new(
            None,
            ObserverPolicy::default(),
            Box::new(|_| Ok(None)),
            1,
            "startup cleanup fixture".to_owned(),
        );
        session.status(Status::Connecting { attempt: 1 }).unwrap();
        // Model an interrupted `run` without a returned NativeObserver. This
        // fixture starts no platform work and claims no authenticated report.
        session.started = true;
        session.native_started.set(native_started);
        session.terminal = Some(ObserverError::Application);
        session
    }

    #[test]
    fn pre_renderer_failure_cleans_and_permits_a_fresh_attempt() {
        let runtime = RuntimeBuilder::current_thread().build().unwrap();
        let cx = runtime.request_cx_with_budget(Budget::INFINITE);
        let deadline = Deadline::after(&cx, Duration::from_secs(1)).unwrap();
        let mut session = failed_startup(false);

        // Entered application startup alone must not turn an approval or
        // display-selection failure into an unconfirmed native cleanup.
        assert_eq!(runtime.block_on(session.cleanup(&cx, deadline)), Ok(()));
        assert!(session.cleanup_confirmed());
        assert_eq!(session.terminal_failure(), Some(ObserverError::Application));
        session.status(Status::Connecting { attempt: 2 }).unwrap();
        assert!(!session.cleanup_confirmed());
        assert_eq!(session.terminal_failure(), None);
        // A fresh dial that never reaches the renderer is also safely reaped.
        assert_eq!(runtime.block_on(session.cleanup(&cx, deadline)), Ok(()));
        assert!(session.cleanup_confirmed());
    }

    #[test]
    fn interrupted_renderer_without_an_owner_cannot_confirm_cleanup_or_retry() {
        let runtime = RuntimeBuilder::current_thread().build().unwrap();
        let cx = runtime.request_cx_with_budget(Budget::INFINITE);
        let deadline = Deadline::after(&cx, Duration::from_secs(1)).unwrap();
        let mut session = failed_startup(true);

        for _ in 0..2 {
            assert_eq!(
                runtime.block_on(session.cleanup(&cx, deadline)),
                Err(CallbackError)
            );
            assert!(!session.cleanup_confirmed());
            assert_eq!(
                session.status(Status::Connecting { attempt: 2 }),
                Err(CallbackError)
            );
            assert_eq!(session.terminal_failure(), Some(ObserverError::Application));
        }
    }
}
