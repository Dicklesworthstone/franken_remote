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
use super::{
    HevcDecoder, PicturePoint, Surface, TEXT_COMPOSER_FLAG, TextCommit, TextRetireMode,
    TextRetirement, TextTaken, Window, WindowEvent, clipboard, event_time, files, picker, sharing,
};
use crate::NativeError;
use crate::macos_control::{Intent, Key, Route};
use crate::macos_scroll::{CaptureClock, Refusal as ScrollRefusal, Scroll};
use crate::macos_switch::{Intent as DisplayIntent, Mode as DisplayMode};
use asupersync::{
    cx::Cx,
    time::{Sleep, sleep},
};
use fr_client::input::{ClientInstant, viewport::SurfaceRect};
use fr_core::input::{DesktopPoint, InputBounds, KeyTransition, PhysicalKey, PointerButton};
use fr_core::input_submission::{Capabilities, Capability};
use fr_media::{
    access_unit::EncodedAccessUnit,
    freshness::ClockPolicy,
    hevc::DecoderRecord,
    worker::{
        Configuration as WorkerConfiguration, Error as WorkerError, overlay::Update as CursorUpdate,
    },
};
use fr_wire::{
    display::{Catalog, Display},
    negotiation::Offer,
};
use frd::{
    media::PresentationStage,
    native_connection::reconnect::{Application, CallbackError, CompletionAction, Status},
    session_startup::{
        ControlledViewer, InteractiveViewerState, NativeObserver, ObserverError, ObserverPolicy,
        Presentation, StreamingViewerControl, Viewer,
        viewer_events::{
            Error as CaptureError, Event, Layout, LocalPoint, MAX_EVENT_AGE_US, NativeCapture,
            PositionedAction, Source, TextContext,
        },
    },
    worker::{Deadline, Launch, LocalPresenter},
};
use std::{
    cell::{Cell, Ref, RefCell},
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
pub use super::clipboard::Report as ClipboardReport;
/// The control request this session makes. Text and each scroll mode require
/// both peers to select their optional capability; a legacy host still grants
/// the supported remainder of this profile.
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
                .with(Capability::LineScroll)
                .with(Capability::PixelScroll)
                .with(Capability::Text),
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
#[derive(Debug, Default, Clone)]
pub struct Report {
    pub clipboard: ClipboardReport,
    pub attempts: u8,
    pub opened: u32,
    pub presented: u64,
    pub hardware_decode: Option<bool>,
    pub approval_requested: bool,
    pub control_requested: bool,
    pub control_granted: bool,
    pub control_requests: u32,
    pub viewing_reconnects: u32,
    pub display_changes: u32,
    pub display_change_closes: Vec<DisplayChangeReport>,
    pub keys: u64,
    pub pointer: u64,
    pub buttons: u64,
    pub scrolls: u64,
    /// Native scroll events refused locally before any action was queued.
    pub scroll_unsupported: u64,
    pub scroll_expired: u64,
    pub scroll_invalid: u64,
    pub input_results: u64,
    /// Whole Unicode values admitted to the original local source, not OS effects.
    pub text_queued: u64,
    pub text_refused: u64,
    pub input_receipts: crate::macos_input_receipts::Summary,
    pub local_stop: Option<LocalStop>,
    /// Actual host exchange stages; absence never confirms remote cleanup.
    pub close_exchange: Option<fr_transport::quic::CloseOutcome>,
    pub lease_revocation: Option<fr_wire::lease_revoked::Revoked>,
}

/// Original terminal evidence retained before opening another display.
#[derive(Debug, Clone, Copy)]
pub struct DisplayChangeReport {
    pub attempt: u8,
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
    binding: fr_wire::input_result::ResultBinding,
    layout: Layout,
    picture: (u32, u32),
    stop: Arc<AtomicBool>,
    released: Arc<AtomicBool>,
    keys: [bool; 256],
    buttons: [bool; 5],
    scroll: Scroll,
    capture_clock: CaptureClock,
    text_context: Option<TextContext>,
    text_serial: Option<u64>,
    text_until: u64,
}
impl Drop for Input {
    fn drop(&mut self) {
        self.source.stop();
        self.released.store(true, Ordering::SeqCst);
    }
}
impl Input {
    fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst) || self.source.control().is_stopped()
    }

    /// Sample the real original input owner's send bound. This does not mint a
    /// ticket; the source context and final admission still check continuity.
    fn refresh_text(&mut self, viewer: &mut ControlledViewer) -> Result<(), ()> {
        if viewer.input_result_binding() != self.binding {
            return Err(());
        }
        let until = viewer
            .input_capture_context()
            .ok()
            .map(|(_, until)| until.0);
        // The check above publishes any actual suspension before this token is
        // read. A compositor gap keeps the same opaque context but disables Send.
        self.text_status();
        self.text_until = if self.text_context.is_some() {
            until.unwrap_or(0)
        } else {
            0
        };
        Ok(())
    }

    /// Keep native composition attached to its original source and uninterrupted
    /// view. A compositor gap disables Send without cancelling a valid draft.
    fn text_status(&mut self) -> (Option<u64>, u64, bool) {
        if self.stopped() || !self.source.supports_text() {
            self.text_context = None;
            self.text_until = 0;
            return (None, 0, false);
        }
        let Ok(context) = self.source.text_context() else {
            self.text_context = None;
            self.text_until = 0;
            return (None, 0, false);
        };
        if context != self.text_context {
            self.text_context = context;
            self.text_until = 0;
            self.text_serial = self.text_serial.and_then(|serial| serial.checked_add(1));
        }
        let (Some(context), Some(serial)) = (&self.text_context, self.text_serial) else {
            return (None, 0, false);
        };
        let ready = self.source.text_context_ready(context).unwrap_or(false)
            && self.source.clock().is_ok_and(|now| now.0 < self.text_until)
            && !self.keys.iter().any(|held| *held)
            && !self.buttons.iter().any(|held| *held);
        (Some(serial), if ready { self.text_until } else { 0 }, ready)
    }

    /// Consume one explicit Send, never a draft/preedit update. A refusal cannot
    /// choose a replacement context or retime and replay the original gesture.
    fn deliver_text(&mut self, commit: TextCommit, report: &mut Report) -> Result<bool, ()> {
        let admitted = (|| {
            if self.stopped() {
                return Err(());
            }
            if self.text_serial != Some(commit.context)
                || self.keys.iter().any(|held| *held)
                || self.buttons.iter().any(|held| *held)
            {
                return Ok(false);
            }
            let Some(context) = self.text_context.as_ref() else {
                return Ok(false);
            };
            let now = self.source.clock().map_err(|_| ())?;
            let Some(sampled) = self.capture_clock.map(commit.captured).map(ClientInstant) else {
                return Ok(false);
            };
            if now.0 >= commit.until
                || now
                    .0
                    .checked_sub(sampled.0)
                    .is_none_or(|age| age >= MAX_EVENT_AGE_US)
            {
                return Ok(false);
            }
            self.scroll.reset();
            match self.source.commit_text_in_context_until(
                context,
                commit.text.as_str(),
                sampled,
                ClientInstant(commit.until),
            ) {
                Ok(()) => Ok(true),
                Err(
                    CaptureError::UnsupportedText
                    | CaptureError::StaleTextContext
                    | CaptureError::TextNotReady,
                ) => Ok(false),
                Err(_) => Err(()),
            }
        })();
        // The source owns its bounded copy, or the whole original Send refused.
        drop(commit);
        if admitted == Ok(true) {
            report.text_queued = report.text_queued.saturating_add(1);
        } else {
            report.text_refused = report.text_refused.saturating_add(1);
        }
        admitted
    }

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
        let now = self.source.clock().map_err(|_| ())?;
        let captured = match *event {
            WindowEvent::Key { captured, .. } => captured,
            WindowEvent::Pointer(at)
            | WindowEvent::Button { at, .. }
            | WindowEvent::Scroll { at, .. } => at.captured,
            _ => return Ok(()),
        };
        let mut sampled = ClientInstant(self.capture_clock.map(captured.ok_or(())?).ok_or(())?);
        if !matches!(event, WindowEvent::Scroll { .. }) {
            // A fractional scroll never crosses a key, move or button action.
            self.scroll.reset();
        }
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
                let Some((event, original)) =
                    self.scroll_event((dx, dy), precise, at, (sampled, now), report)
                else {
                    return Ok(());
                };
                sampled = original;
                event
            }
            _ => return Ok(()),
        };
        self.source.push(event, sampled).map_err(|_| ())
    }

    fn scroll_event(
        &mut self,
        distance: (f64, f64),
        precise: bool,
        at: PicturePoint,
        (sampled, now): (ClientInstant, ClientInstant),
        report: &mut Report,
    ) -> Option<(Event, ClientInstant)> {
        if !at.inside {
            self.scroll.reset();
            return None;
        }
        let delta = match self.scroll.push(
            distance,
            precise,
            self.source.capabilities(),
            sampled.0,
            now.0,
        ) {
            Ok(Some(delta)) => delta,
            Ok(None) => return None,
            Err(ScrollRefusal::Unsupported) => {
                report.scroll_unsupported += 1;
                return None;
            }
            Err(ScrollRefusal::Expired) => {
                report.scroll_expired += 1;
                return None;
            }
            Err(ScrollRefusal::Invalid | ScrollRefusal::OutOfRange) => {
                report.scroll_invalid += 1;
                return None;
            }
        };
        report.scrolls += 1;
        let event = Event::Positioned {
            location: self.layout.at(self.local(at)),
            action: PositionedAction::Scroll {
                x: delta.x,
                y: delta.y,
                unit: delta.unit,
            },
        };
        Some((event, ClientInstant(delta.sampled)))
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

// These facts have independent owners: chooser configuration, actual service
// completion, startup ownership and positively completed cleanup.
#[allow(clippy::struct_excessive_bools)]
pub struct Session {
    control: Option<ControlPolicy>,
    policy: ObserverPolicy,
    choose: Choose,
    display_picker: bool,
    picker: Rc<RefCell<Option<picker::DisplayPicker>>>,
    picker_error: Rc<Cell<Option<picker::Error>>>,
    close_cx: Option<Cx>,
    epoch: u128,
    title: String,
    fit_window: Option<(u32, u32)>,
    window: WindowSlot,
    /// The started observer, reaped in `cleanup`. `run` and `cleanup` both
    /// take `&mut self`, so `run`'s borrow has ended before cleanup starts.
    observer: Option<NativeObserver>,
    input: Rc<RefCell<Option<Input>>>,
    files: Rc<RefCell<files::Files>>,
    clipboard: Rc<RefCell<clipboard::Clipboard>>,
    report: Rc<RefCell<Report>>,
    closed: Rc<Cell<bool>>,
    intent: Rc<RefCell<Intent>>,
    display_intent: Rc<RefCell<DisplayIntent>>,
    service_succeeded: bool,
    started: bool,
    /// Set inside the deferred renderer, after approval and display selection.
    /// Entering `run` alone does not mean any native cleanup owner exists.
    native_started: Rc<Cell<bool>>,
    cleanup_confirmed: bool,
    terminal: Option<ObserverError>,
}
impl Drop for Session {
    fn drop(&mut self) {
        // Fence before field destruction can free a native window or enter IME
        // teardown. This does not claim that mandatory cleanup completed.
        if let Some(observer) = &self.observer {
            observer.control().stop();
        }
        if let Ok(mut input) = self.input.try_borrow_mut() {
            input.take();
        }
    }
}
impl Session {
    /// `control` None observes; Some consumes the invocation's initial control
    /// choice once. Recovered viewing requires a fresh local F8 choice.
    /// `choose` picks the host display; `with_display_picker` instead uses a
    /// native choice over each fresh approved catalog. `epoch` must be nonzero.
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
            display_picker: false,
            picker: Rc::default(),
            picker_error: Rc::default(),
            close_cx: None,
            epoch,
            title,
            fit_window: None,
            window: Rc::default(),
            observer: None,
            input: Rc::default(),
            files: Rc::default(),
            clipboard: Rc::default(),
            report: Rc::default(),
            closed: Rc::default(),
            intent: Rc::new(RefCell::new(Intent::new(control.is_some()))),
            display_intent: Rc::default(),
            service_succeeded: false,
            started: false,
            native_started: Rc::default(),
            cleanup_confirmed: false,
            terminal: None,
        }
    }
    /// Opt into the initial controlling attempt's shared text clipboard.
    #[must_use]
    pub fn clipboard(self, enabled: bool) -> Self {
        self.clipboard
            .borrow_mut()
            .request(enabled && self.control.is_some());
        self
    }
    /// An optional logical point bound for the fitted viewer content.
    #[must_use]
    pub fn with_fit_window(mut self, fit: Option<(u32, u32)>) -> Self {
        self.fit_window = fit;
        self
    }
    /// Choose from the original approved catalog on every new attempt.
    #[must_use]
    pub fn with_display_picker(mut self) -> Self {
        self.display_picker = true;
        self
    }
    /// Independently live cleanup context on this session's runtime clock,
    /// used only for the original granted controller's terminal exchange.
    #[must_use]
    pub fn with_cleanup_context(mut self, cx: Cx) -> Self {
        self.close_cx = Some(cx);
        self
    }
    /// Actual native selection failure, retained before cleanup closes a picker.
    pub fn picker_error(&self) -> Option<picker::Error> {
        self.picker_error.get()
    }
    /// Explicit choices belong to this invocation's first configured observer.
    /// Recovered viewing and a fresh F8 gesture never repeat the selection.
    #[must_use]
    pub fn with_files(mut self, files: files::Files) -> Self {
        self.files = Rc::new(RefCell::new(files));
        self
    }
    /// Bounded, content-free results of the original selected operations.
    pub fn files(&self) -> Ref<'_, files::Files> {
        self.files.borrow()
    }
    /// Select the original foreground owner's comparison identity before file
    /// setup. An already configured or cancelled endpoint cannot be rebound.
    pub fn file_command_connection(
        &mut self,
        connection: &str,
    ) -> Result<(), frd::native_files::local::Error> {
        self.files.borrow_mut().command_connection(connection)
    }
    pub fn report(&self) -> Report {
        let mut report = self.report.borrow().clone();
        let mut clipboard = self.clipboard.borrow_mut();
        clipboard.collect();
        report.clipboard = clipboard.report();
        report
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

/// The original local endpoint's selection worker may still be inside a
/// filesystem call after intake is fenced. Keep its owner and block another
/// attempt until both fixed workers exit, under the same cleanup deadline.
async fn reap_file_commands(files: &RefCell<files::Files>, cx: &Cx, deadline: Deadline) -> bool {
    loop {
        if files.borrow().commands_complete() {
            return true;
        }
        let Some(timer) = cx.timer_driver() else {
            return false;
        };
        let now = timer.now();
        if cx.checkpoint().is_err() || now >= deadline.time() {
            return false;
        }
        let remaining = Duration::from_nanos(deadline.time().as_nanos() - now.as_nanos());
        sleep(now, PUMP.min(remaining)).await;
    }
}

async fn reap_clipboard_watchdog(
    clipboard: &RefCell<clipboard::Clipboard>,
    cx: &Cx,
    deadline: Deadline,
) -> bool {
    loop {
        if clipboard.borrow_mut().finished() {
            return true;
        }
        let Some(timer) = cx.timer_driver() else {
            return false;
        };
        let now = timer.now();
        if cx.checkpoint().is_err() || now >= deadline.time() {
            return false;
        }
        sleep(now, Duration::from_millis(1)).await;
    }
}

/// Parking does no native cleanup. Only a fenced owner can request and
/// positively complete teardown; a prior negative does not invalidate cleanup.
fn retire_text(window: &RefCell<Option<Window>>, mode: TextRetireMode) -> Result<(), ()> {
    let expected = match mode {
        TextRetireMode::Park => TextRetirement::Parked,
        TextRetireMode::Fenced => TextRetirement::Clean,
    };
    let outcome = window.borrow().as_ref().map_or(Ok(expected), |opened| {
        opened.text_stopped(mode).map_err(|_| ())
    })?;
    if outcome != expected {
        return Err(());
    }
    Ok(())
}

/// Select terminal batch evidence before any follow-up native call.
fn terminal_event(events: &[WindowEvent]) -> Option<LocalStop> {
    if events.contains(&WindowEvent::CloseRequested) {
        Some(LocalStop::WindowClosed)
    } else if events.iter().any(|event| {
        matches!(*event, WindowEvent::Key { usage, pressed, modifiers, .. }
            if crate::macos_control::emergency_escape(usage, pressed, modifiers))
    }) {
        Some(LocalStop::LocalEscape)
    } else if events.contains(&WindowEvent::FocusLost) {
        Some(LocalStop::FocusLost)
    } else if events.contains(&WindowEvent::Overflow) {
        Some(LocalStop::Overflow)
    } else {
        None
    }
}

/// Drain the window's events, deliver them to a granted session, and stop
/// control (or the session) on the local conditions frd requires.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn service(
    window: &RefCell<Option<Window>>,
    input: &RefCell<Option<Input>>,
    report: &RefCell<Report>,
    intent: &RefCell<Intent>,
    display_intent: &RefCell<DisplayIntent>,
    closed: &Cell<bool>,
    stop: &StreamingViewerControl,
    files: &RefCell<files::Files>,
    clipboard: &RefCell<clipboard::Clipboard>,
    sharing: &mut sharing::Ui,
    events: &mut Vec<WindowEvent>,
) {
    events.clear();
    let halt = |reason: LocalStop, input: &mut Option<Input>, report: &mut Report| {
        report.local_stop.get_or_insert(reason);
        display_intent.borrow_mut().cancel();
        *input = None;
        stop.stop();
        let _ = retire_text(window, TextRetireMode::Fenced);
        files.borrow_mut().cancel();
        clipboard.borrow_mut().stop(window.borrow().as_ref());
    };
    let retire_stopped = || -> Result<bool, ()> {
        let retired = {
            let mut slot = input.borrow_mut();
            if slot.as_ref().is_some_and(Input::stopped) {
                *slot = None;
                true
            } else {
                false
            }
        };
        if !retired {
            return Ok(false);
        }
        files.borrow_mut().cancel();
        intent.borrow_mut().invalidate();
        let pending = display_intent.borrow().pending();
        if !pending {
            stop.stop();
        }
        // A pending explicit close has already parked its original producer.
        let mode = if pending {
            TextRetireMode::Park
        } else {
            TextRetireMode::Fenced
        };
        retire_text(window, mode)?;
        Ok(!pending)
    };
    match retire_stopped() {
        Ok(true) => return,
        Ok(false) => {}
        Err(()) => {
            halt(
                LocalStop::NativeFailure,
                &mut input.borrow_mut(),
                &mut report.borrow_mut(),
            );
            return;
        }
    }
    let terminal = {
        let mut slot = window.borrow_mut();
        let Some(opened) = slot.as_mut() else {
            return;
        };
        // A parked producer never republishes its old context or allows native
        // editor cleanup before the original close fences its owners.
        let text_updated = if display_intent.borrow().pending() {
            true
        } else {
            // Drop every source/queue borrow before AppKit. An ordinary update
            // cannot acknowledge a native stop, even when context is absent.
            let (context, until, ready) = input
                .borrow_mut()
                .as_mut()
                .map_or((None, 0, false), Input::text_status);
            opened.text_update(context, until, ready).is_ok()
        };
        files.borrow().file_drop_ready(
            input
                .borrow()
                .as_ref()
                .is_some_and(|input| !input.stopped()),
        );
        if !text_updated
            || opened.file_selection_update().is_err()
            || opened
                .display_change_enabled(display_intent.borrow().enabled())
                .is_err()
        {
            Some(LocalStop::NativeFailure)
        } else {
            opened.poll(Duration::ZERO, events);
            // Native terminal callbacks defer IME cleanup. Fence owners before
            // state, clipboard or sharing can enter another native call.
            terminal_event(events)
        }
    };
    if terminal.is_none() {
        match retire_stopped() {
            Ok(true) => return,
            Ok(false) => {}
            Err(()) => {
                halt(
                    LocalStop::NativeFailure,
                    &mut input.borrow_mut(),
                    &mut report.borrow_mut(),
                );
                return;
            }
        }
    }
    let mut report = report.borrow_mut();
    let mut input = input.borrow_mut();
    let mut intent = intent.borrow_mut();
    if let Some(reason) = terminal {
        if matches!(reason, LocalStop::WindowClosed | LocalStop::LocalEscape) {
            closed.set(true);
            intent.invalidate();
            halt(reason, &mut input, &mut report);
        } else {
            intent.suspend();
            if reason == LocalStop::NativeFailure
                || input.is_some()
                || intent.requested()
                || display_intent.borrow().pending()
            {
                halt(reason, &mut input, &mut report);
            } else {
                // No input owner exists. Keep viewing, but discard old local
                // composition and any display choice across focus/queue loss.
                display_intent
                    .borrow_mut()
                    .set_mode(DisplayMode::Disabled, event_time());
                let _ = retire_text(window, TextRetireMode::Fenced);
            }
        }
        return;
    }
    let slot = window.borrow();
    let Some(opened) = slot.as_ref() else {
        drop(slot);
        intent.invalidate();
        halt(LocalStop::NativeFailure, &mut input, &mut report);
        return;
    };
    let state = opened.state().ok();
    files
        .borrow()
        .file_drop_ready(input.is_some() && state.is_some_and(super::WindowState::input_ready));
    let display_action = opened.take_display_change();
    drop(slot);
    let reason = if display_action.is_err() {
        Some(LocalStop::NativeFailure)
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
        if input.is_some()
            || intent.requested()
            || (display_intent.borrow().pending() && reason != LocalStop::PictureChanged)
        {
            halt(reason, &mut input, &mut report);
            return;
        }
        // No input owner remains; native cleanup cannot delay an input fence.
        let _ = retire_text(window, TextRetireMode::Fenced);
        // A view without input can choose another display before its first
        // picture. Focus, hiding and invalid native state never carry a click
        // forward to a later eligible window.
        if reason != LocalStop::PictureChanged {
            display_intent
                .borrow_mut()
                .set_mode(DisplayMode::Disabled, event_time());
            return;
        }
    }
    clipboard.borrow_mut().turn(window.borrow().as_ref());
    let sharing_ok = window
        .borrow()
        .as_ref()
        .is_some_and(|window| sharing.turn(window, &mut files.borrow_mut()).is_ok());
    if !sharing_ok {
        intent.invalidate();
        halt(LocalStop::NativeFailure, &mut input, &mut report);
        return;
    }
    let now = event_time();
    if let Ok(Some(captured)) = display_action
        && display_intent.borrow_mut().request(captured, now)
    {
        // Park this exact producer; dropping Source would cancel the original
        // session before its orderly close callback can fence the actual grant.
        // No new event enters it after this turn, including the click itself.
        intent.retire_initial();
        if retire_text(window, TextRetireMode::Park).is_err() {
            halt(LocalStop::NativeFailure, &mut input, &mut report);
            return;
        }
        files.borrow_mut().cancel();
        clipboard.borrow_mut().stop(window.borrow().as_ref());
    }
    let pending = display_intent.borrow().pending();
    let expired = display_intent.borrow().expired(now);
    if pending {
        intent.invalidate();
        if expired {
            halt(LocalStop::NativeFailure, &mut input, &mut report);
        }
        return;
    }
    if reason.is_some() {
        return;
    }
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
    // Native queries above can synchronously discover a new terminal event.
    // Probe/take before any Source delivery; the owned text still follows the
    // earlier physical batch, and remains exclusive until its acknowledgment.
    let text = match window.borrow().as_ref() {
        Some(opened) => opened.take_text(),
        None => return,
    };
    if text.is_err() {
        report.text_refused = report.text_refused.saturating_add(1);
        intent.invalidate();
        halt(LocalStop::NativeFailure, &mut input, &mut report);
        return;
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
    let completion = match text {
        Ok(TextTaken::Commit(commit)) => {
            let context = commit.context;
            let admitted = if let Some(granted) = input.as_mut() {
                granted.deliver_text(commit, &mut report)
            } else {
                report.text_refused = report.text_refused.saturating_add(1);
                Ok(false)
            };
            let Ok(admitted) = admitted else {
                intent.invalidate();
                halt(LocalStop::NativeFailure, &mut input, &mut report);
                return;
            };
            Some((context, admitted))
        }
        Ok(TextTaken::Refused { context }) => {
            report.text_refused = report.text_refused.saturating_add(1);
            Some((context, false))
        }
        Ok(TextTaken::Empty) => None,
        Err(_) => {
            report.text_refused = report.text_refused.saturating_add(1);
            intent.invalidate();
            halt(LocalStop::NativeFailure, &mut input, &mut report);
            return;
        }
    };
    if let Some((context, admitted)) = completion {
        let completed = window
            .borrow()
            .as_ref()
            .is_some_and(|opened| opened.text_complete(context, admitted).is_ok());
        if !completed {
            intent.invalidate();
            halt(LocalStop::NativeFailure, &mut input, &mut report);
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
    window.claim_input().map_err(|_| ())?;
    let binding = viewer.input_result_binding();
    let stop = Arc::new(AtomicBool::new(false));
    let released = Arc::new(AtomicBool::new(false));
    let capture = Capture {
        stop: stop.clone(),
        released: released.clone(),
    };
    viewer
        .capture_input_owned(&layout.clone(), |source| {
            let sampled = source.clock().map_err(|_| ())?;
            let capture_clock = CaptureClock::new(event_time().ok_or(())?, sampled.0);
            *slot.borrow_mut() = Some(Input {
                source,
                binding,
                layout,
                picture: (width, height),
                stop,
                released,
                keys: [false; 256],
                buttons: [false; 5],
                scroll: Scroll::new(MAX_EVENT_AGE_US),
                capture_clock,
                text_context: None,
                text_serial: Some(0),
                text_until: 0,
            });
            Ok::<_, ()>(capture)
        })
        .map_err(|_| ())?;
    Ok(true)
}

impl Application for Session {
    type Output = ();
    fn offer(&mut self, _attempt: u8, original: &Offer) -> Result<Offer, CallbackError> {
        let available = self
            .files
            .borrow()
            .offer(self.clipboard.borrow().available());
        let keep = |name: &str| {
            !matches!(
                name,
                fr_wire::attachment::FILES_CAPABILITY
                    | fr_wire::files::CAPABILITY
                    | fr_wire::files::CHANNEL_SCOPE_CAPABILITY
                    | fr_wire::files::DOWNLOAD_CAPABILITY
                    | fr_wire::files::DUPLEX_CAPABILITY
                    | fr_wire::attachment::CLIPBOARD_CAPABILITY
                    | fr_wire::clipboard::CAPABILITY
                    | fr_wire::clipboard::startup::CAPABILITY
            ) || available.capabilities.iter().any(|cap| cap.name == name)
        };
        if original
            .capabilities
            .iter()
            .any(|cap| cap.required && !keep(&cap.name))
        {
            return Err(CallbackError);
        }
        // Remove only unavailable optional file/clipboard choices. Keep the
        // original role, limits, required gates and other options exact.
        let mut offer = original.clone();
        offer.capabilities.retain(|cap| keep(&cap.name));
        if self
            .control
            .is_some_and(|policy| policy.capabilities.contains(Capability::PixelScroll))
            && offer.role == fr_wire::negotiation::Role::RequestControl
        {
            offer = fr_wire::control::with_pixel_scroll(offer).map_err(|_| CallbackError)?;
        }
        if self
            .control
            .is_some_and(|policy| policy.capabilities.contains(Capability::Text))
            && offer.role == fr_wire::negotiation::Role::RequestControl
        {
            offer = fr_wire::control::with_committed_text(offer).map_err(|_| CallbackError)?;
        }
        Ok(offer)
    }
    #[allow(clippy::too_many_lines)] // one attempt: open, serve, control
    async fn run(&mut self, attempt: u8, viewer: Viewer) -> Result<(), ObserverError> {
        if self.started || self.native_started.get() || self.observer.is_some() {
            return Err(ObserverError::Order);
        }
        self.started = true;
        self.service_succeeded = false;
        self.cleanup_confirmed = false;
        self.report.borrow_mut().attempts = attempt;
        let stop = viewer.control();
        let window = self.window.clone();
        let input = self.input.clone();
        let files = self.files.clone();
        let clipboard = self.clipboard.clone();
        let clipboard_requested = clipboard.borrow().available();
        let file_directions = sharing::flags(&files.borrow()).count_ones();
        let file_selections = files.borrow().file_drop_requested();
        let text_requested = self
            .control
            .is_some_and(|policy| policy.capabilities.contains(Capability::Text));
        let sharing_flags = sharing::flags(&files.borrow())
            | 4
            | (u32::from(clipboard_requested) << 3)
            | if file_selections {
                super::FILE_SELECTION_FLAG
            } else {
                0
            }
            | if text_requested {
                TEXT_COMPOSER_FLAG
            } else {
                0
            };
        let report = self.report.clone();
        let closed = self.closed.clone();
        let intent = self.intent.clone();
        let display_intent = self.display_intent.clone();
        let close_cx = self.close_cx.clone();
        let switchable = self.control.is_none() || close_cx.is_some();
        display_intent.borrow_mut().set_mode(
            if switchable {
                DisplayMode::Viewing
            } else {
                DisplayMode::Disabled
            },
            event_time(),
        );
        let hardware = Hardware::default();
        let epoch = self
            .epoch
            .checked_add(u128::from(attempt))
            .filter(|epoch| *epoch != 0)
            .ok_or(ObserverError::InvalidConfiguration)?;
        let title = self.title.clone();
        let fit_window = self.fit_window;
        let control = self.control;
        let choose = &mut self.choose;
        let select_fresh_display = self.display_picker;
        let picker = self.picker.clone();
        let picker_error = self.picker_error.clone();
        let window_slot = window.clone();
        let clipboard_ui = clipboard.clone();
        let file_ui = files.clone();
        let decoder_hardware = hardware.clone();
        let approval_report = report.clone();
        let native_started = self.native_started.clone();
        let opening = viewer.observe_with_renderer(
            move |display, _original| async move {
                let points = crate::macos_sizing::sharing_points_with_selections(
                    (display.pixel_width, display.pixel_height),
                    (display.logical_width, display.logical_height),
                    fit_window,
                    file_directions,
                    clipboard_requested,
                    text_requested,
                    file_selections,
                )?;
                // From this point onward, losing the observer is not proof
                // that AppKit, the renderer or its worker finished cleanup.
                native_started.set(true);
                let opened = Window::open_with_sharing(
                    display.pixel_width,
                    display.pixel_height,
                    points,
                    &title,
                    sharing_flags,
                )
                .map_err(|_| ())?;
                clipboard_ui.borrow_mut().install(&opened).map_err(|_| ())?;
                if let Some(ingress) = file_ui.borrow().file_drop_ingress() {
                    opened.file_selection_bind(ingress).map_err(|_| ())?;
                }
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
            move |catalog| {
                if !select_fresh_display {
                    return choose(catalog);
                }
                let result = (|| {
                    let mut owner = picker.borrow_mut();
                    if owner.is_none() {
                        *owner = Some(picker::DisplayPicker::open(catalog)?);
                    }
                    owner
                        .as_mut()
                        .ok_or(picker::Error::NativeFailure)?
                        .poll(catalog)
                })();
                result.map_err(|error| picker_error.set(Some(error)))
            },
            move |_notice| {
                approval_report.borrow_mut().approval_requested = true;
                Ok(())
            },
        );
        let mut events = Vec::with_capacity(64);
        let mut sharing = sharing::Ui::default();
        let mut pump = || {
            service(
                &window,
                &input,
                &report,
                &intent,
                &display_intent,
                &closed,
                &stop,
                &files,
                &clipboard,
                &mut sharing,
                &mut events,
            );
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
        files.borrow_mut().configure(observer);
        clipboard.borrow_mut().configure(
            observer,
            window.borrow().as_ref(),
            self.close_cx.clone().or_else(Cx::current),
        );
        let count = report.clone();
        let frames = move |frame: Option<Presentation>| {
            if frame.is_some() {
                count.borrow_mut().presented += 1;
            }
        };
        let outcome = match control {
            None => {
                let closing = closed.clone();
                let switching = display_intent.clone();
                let ui_window = window.clone();
                let served = observer.serve_until(move |frame| {
                    frames(frame);
                    let eligible = ui_window
                        .borrow()
                        .as_ref()
                        .and_then(|window| window.state().ok())
                        .is_some_and(|state| state.key && state.visible);
                    switching.borrow_mut().set_mode(
                        if eligible {
                            DisplayMode::Viewing
                        } else {
                            DisplayMode::Disabled
                        },
                        event_time(),
                    );
                    if switching.borrow_mut().reject_ineligible(eligible) {
                        return Err(());
                    }
                    Ok(if closing.get() || switching.borrow_mut().begin_close() {
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
                let switching = display_intent.clone();
                let results = report.clone();
                let mut ui = move |state: InteractiveViewerState<'_>, frame| {
                    frames(frame);
                    let eligible = switchable
                        && ui_window
                            .borrow()
                            .as_ref()
                            .and_then(|window| window.state().ok())
                            .is_some_and(|state| state.key && state.visible);
                    let mode = match &state {
                        InteractiveViewerState::Viewing(_) if eligible => DisplayMode::Viewing,
                        InteractiveViewerState::Controlled(_) if eligible => {
                            DisplayMode::Controlled
                        }
                        _ => DisplayMode::Disabled,
                    };
                    switching.borrow_mut().set_mode(mode, event_time());
                    if switching
                        .borrow_mut()
                        .reject_ineligible(mode != DisplayMode::Disabled)
                    {
                        return Err(());
                    }
                    if switching.borrow().pending() {
                        if matches!(&state, InteractiveViewerState::Requesting(_)) {
                            switching.borrow_mut().cancel();
                            return Err(());
                        }
                        if switching.borrow_mut().begin_close() {
                            return Ok(std::ops::ControlFlow::Break(
                                fr_wire::closure::Reason::Requested,
                            ));
                        }
                        return Err(());
                    }
                    let fresh = shown(&ui_window, frame).filter(|f| last_shown != Some(*f));
                    match state {
                        InteractiveViewerState::Viewing(view) => {
                            let Some(frame) = fresh.or(last_shown) else {
                                ui_intent.borrow_mut().take_request(false, event_time());
                                return Ok(std::ops::ControlFlow::Continue(()));
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
                            if ui_intent
                                .borrow_mut()
                                .take_request(local_ready && view.evidence().is_ok(), event_time())
                            {
                                let mut report = ui_report.borrow_mut();
                                report.control_requested = true;
                                report.control_requests += 1;
                                drop(report);
                                switching
                                    .borrow_mut()
                                    .set_mode(DisplayMode::Disabled, event_time());
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
                            if let Some(input) = ui_input.borrow_mut().as_mut() {
                                input.refresh_text(viewer)?;
                            }
                        }
                    }
                    Ok(std::ops::ControlFlow::Continue(()))
                };
                let result = move |result| {
                    let mut report = results.borrow_mut();
                    report.input_results = report.input_results.saturating_add(1);
                    report.input_receipts.observe(result);
                };
                if let Some(cleanup) = &close_cx {
                    let served = observer.serve_interactive_session_until(
                        cleanup,
                        policy.sequence,
                        policy.capabilities,
                        policy.input,
                        ui,
                        result,
                    );
                    pumped(served, &mut pump).await.map(|_| ())
                } else {
                    // Existing library callers without independent cleanup
                    // retain their immediate-stop service; no switch is armed.
                    let served = observer.serve_interactive_control(
                        policy.sequence,
                        policy.capabilities,
                        policy.input,
                        move |state, frame| {
                            ui(state, frame).and_then(|decision| match decision {
                                std::ops::ControlFlow::Continue(()) => Ok(()),
                                std::ops::ControlFlow::Break(_) => Err(()),
                            })
                        },
                        result,
                    );
                    pumped(served, &mut pump).await
                }
            }
        };
        self.service_succeeded = outcome.is_ok();
        outcome
    }
    fn cleanup(
        &mut self,
        cx: &Cx,
        deadline: Deadline,
    ) -> impl Future<Output = Result<(), CallbackError>> {
        // Fence at call time, before any AppKit/IME cleanup can block.
        self.input.borrow_mut().take();
        if let Some(observer) = &self.observer {
            observer.control().stop();
        }
        let text_retired = retire_text(&self.window, TextRetireMode::Fenced).is_ok();
        self.intent.borrow_mut().invalidate();
        self.clipboard
            .borrow_mut()
            .stop(self.window.borrow().as_ref());
        // A failed dial with no configured observer has not spent the local
        // choices. Once configured, every cleanup fences their original owner.
        if self.observer.is_some() {
            self.files.borrow_mut().cancel();
        }
        self.cleanup_confirmed = false;
        // The picker belongs to this original startup, including when the
        // renderer never began. Closing it is actual main-thread cleanup,
        // not a reason to invent local cancellation or remote close evidence.
        let picker_closed = match self.picker.borrow_mut().as_mut() {
            Some(picker) => match picker.close() {
                Ok(()) => true,
                Err(error) => {
                    if self.picker_error.get().is_none() {
                        self.picker_error.set(Some(error));
                    }
                    false
                }
            },
            None => true,
        };
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
                    let files = observer.reap_files(cx, deadline).await;
                    let clipboard = observer.reap_clipboard(cx, deadline).await;
                    for _ in 0..2 {
                        let _ = observer.collect_clipboard();
                        self.clipboard.borrow_mut().collect();
                    }
                    let clipboard = matches!(
                        clipboard,
                        Ok(frd::native_clipboard::Cleanup::NotStarted
                            | frd::native_clipboard::Cleanup::Finished(_))
                    );
                    media.is_ok() && input.is_ok() && files.is_ok() && clipboard
                }
                // Approval and display selection precede the deferred native
                // factory. Their failures have no native owners to reap. Once
                // that factory starts, dropping its future proves no cleanup.
                None => !self.native_started.get(),
            };
            let commands = reap_file_commands(&self.files, cx, deadline).await;
            let clipboard = reap_clipboard_watchdog(&self.clipboard, cx, deadline).await;
            if !result || !text_retired || !picker_closed || !commands || !clipboard {
                return Err(CallbackError);
            }
            self.observer = None;
            window.borrow_mut().take();
            self.picker.borrow_mut().take();
            self.started = false;
            self.native_started.set(false);
            self.cleanup_confirmed = true;
            Ok(())
        }
    }
    fn status(&mut self, status: Status) -> Result<(), CallbackError> {
        match status {
            Status::Connecting { .. } => {
                if self.started
                    || self.native_started.get()
                    || self.observer.is_some()
                    || self.picker.borrow().is_some()
                    || !self.files.borrow().commands_complete()
                    || !self.clipboard.borrow_mut().finished()
                {
                    return Err(CallbackError);
                }
                self.native_started.set(false);
                self.cleanup_confirmed = false;
                self.terminal = None;
                self.picker_error.set(None);
                self.service_succeeded = false;
                self.report.borrow_mut().close_exchange = None;
                self.report.borrow_mut().lease_revocation = None;
                self.intent.borrow_mut().begin_attempt();
                self.display_intent.borrow_mut().begin_attempt();
                Ok(())
            }
            Status::Reconnecting { .. } if self.display_intent.borrow().pending() => {
                Err(CallbackError)
            }
            Status::Reconnecting { .. } | Status::Reopening { .. }
                if self.closed.get() || self.report.borrow().local_stop.is_some() =>
            {
                Err(CallbackError)
            }
            _ => Ok(()),
        }
    }
    fn completion_action(&mut self) -> Result<CompletionAction, CallbackError> {
        if !self.display_intent.borrow().pending() {
            return Ok(CompletionAction::Finish);
        }
        let report = self.report.borrow();
        if !self.service_succeeded
            || !self.cleanup_confirmed
            || self.started
            || self.native_started.get()
            || self.observer.is_some()
            || self.picker.borrow().is_some()
            || self.closed.get()
            || report.local_stop.is_some()
            || report.close_exchange.is_none()
            || self.terminal_failure().is_some()
            || !(1..32).contains(&report.attempts)
            || report.display_change_closes.len() >= 31
        {
            return Err(CallbackError);
        }
        let closed = DisplayChangeReport {
            attempt: report.attempts,
            close_exchange: report.close_exchange,
            lease_revocation: report.lease_revocation,
        };
        drop(report);
        if !self.display_intent.borrow_mut().consume() {
            return Err(CallbackError);
        }
        let mut report = self.report.borrow_mut();
        report.display_changes += 1;
        report.display_change_closes.push(closed);
        self.display_picker = true;
        self.intent.borrow_mut().retire_initial();
        // Original handles remain for the completion report; no new observer
        // can acquire another source or destination from this invocation.
        self.files.borrow_mut().cancel();
        Ok(CompletionAction::Reopen)
    }
    fn terminal_failure(&self) -> Option<ObserverError> {
        self.terminal
            .filter(|error| !self.service_succeeded || !requested_terminal(*error))
    }
}

fn requested_terminal(error: ObserverError) -> bool {
    use frd::session_startup::{ControlledViewerError, Error, StreamingViewerError};
    match error {
        ObserverError::Streaming(StreamingViewerError::Control(
            ControlledViewerError::LeaseRevoked(report),
        )) => {
            matches!(
                report.reason,
                fr_wire::lease_revoked::Reason::ClientRequested
                    | fr_wire::lease_revoked::Reason::SessionEnded
            ) && report.cleanup != fr_wire::lease_revoked::CleanupStage::Failed
        }
        ObserverError::Streaming(StreamingViewerError::Session(Error::RemoteClosed(report))) => {
            report.reason == fr_wire::closure::ClosedReason::ClientRequested
                && report.cleanup != fr_wire::closure::Cleanup::Incomplete
        }
        _ => false,
    }
}

/// Retain the original owner's authenticated result before its cleanup can
/// remove it. Local close, media exit and source release create no host proof.
fn reported_terminal(observer: &NativeObserver) -> Option<ObserverError> {
    use frd::session_startup::{ControlledViewerError, Error, StreamingViewerError};
    let viewing = observer.disconnect_outcome();
    let control = observer.control_disconnect_outcome();
    let revocation = control.and_then(|outcome| outcome.revocation);
    let exchanges = [viewing, control.map(|outcome| outcome.exchange)];
    let lease_error = |report| {
        ObserverError::Streaming(StreamingViewerError::Control(
            ControlledViewerError::LeaseRevoked(report),
        ))
    };
    let session_error = |report| {
        ObserverError::Streaming(StreamingViewerError::Session(Error::RemoteClosed(report)))
    };
    if let Some(report) = revocation
        && (!matches!(
            report.reason,
            fr_wire::lease_revoked::Reason::ClientRequested
                | fr_wire::lease_revoked::Reason::SessionEnded
        ) || report.cleanup == fr_wire::lease_revoked::CleanupStage::Failed)
    {
        return Some(lease_error(report));
    }
    // A normal lease release must not hide a racing adverse session report.
    if let Some(report) = exchanges
        .into_iter()
        .flatten()
        .filter_map(|outcome| outcome.report)
        .find(|report| {
            report.reason != fr_wire::closure::ClosedReason::ClientRequested
                || report.cleanup == fr_wire::closure::Cleanup::Incomplete
        })
    {
        return Some(session_error(report));
    }
    for outcome in exchanges.into_iter().flatten() {
        if let Err(error) = outcome.transport
            && !matches!(
                error,
                fr_transport::quic::Error::Expired
                    | fr_transport::quic::Error::Closed
                    | fr_transport::quic::Error::Backpressure
            )
        {
            return Some(ObserverError::Streaming(StreamingViewerError::Transport(
                error,
            )));
        }
    }
    revocation.map(lease_error).or_else(|| {
        exchanges
            .into_iter()
            .flatten()
            .find_map(|outcome| outcome.report)
            .map(session_error)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::{runtime::RuntimeBuilder, types::Budget};

    fn selected_close() -> Session {
        let mut session = failed_startup(false);
        // Typed completion-policy fixture only: no native window, connection,
        // grant or physical display is created or claimed by these tests.
        session.started = false;
        session.service_succeeded = true;
        session.cleanup_confirmed = true;
        session.terminal = None;
        session.intent = Rc::new(RefCell::new(Intent::new(true)));
        session.report.borrow_mut().attempts = 1;
        session.report.borrow_mut().close_exchange = Some(fr_transport::quic::CloseOutcome {
            request_acknowledged: true,
            report: Some(fr_wire::closure::Closed {
                reason: fr_wire::closure::ClosedReason::ClientRequested,
                cleanup: fr_wire::closure::Cleanup::Complete,
                effects: fr_wire::closure::OutstandingEffects::Unknown,
            }),
            transport: Ok(()),
        });
        let mut intent = session.display_intent.borrow_mut();
        intent.set_mode(DisplayMode::Viewing, Some(Duration::from_millis(1)));
        assert!(intent.request(Duration::from_millis(2), Some(Duration::from_millis(3))));
        assert!(intent.begin_close());
        drop(intent);
        session
    }

    #[test]
    fn explicit_reopen_consumes_one_choice_and_keeps_original_close_evidence() {
        let mut session = selected_close();
        let original = session.report.borrow().close_exchange;
        assert_eq!(session.completion_action(), Ok(CompletionAction::Reopen));
        assert!(session.display_picker);
        assert!(session.intent.borrow().needs_explicit_request());
        let report = session.report();
        assert_eq!(report.display_changes, 1);
        assert_eq!(report.display_change_closes.len(), 1);
        assert_eq!(report.display_change_closes[0].close_exchange, original);
        assert_eq!(session.completion_action(), Ok(CompletionAction::Finish));
        session.status(Status::Connecting { attempt: 2 }).unwrap();
        assert!(
            !session
                .intent
                .borrow_mut()
                .take_request(true, Some(Duration::from_millis(10)))
        );
        assert!(!session.display_intent.borrow().pending());
        assert!(session.report().close_exchange.is_none());
        assert_eq!(
            session.report().display_change_closes[0].close_exchange,
            original
        );
    }

    #[test]
    fn uncertain_cleanup_failed_service_adverse_terminal_and_local_stop_prevent_reopen() {
        for refusal in 0..7 {
            let mut session = selected_close();
            match refusal {
                0 => session.cleanup_confirmed = false,
                1 => session.service_succeeded = false,
                2 => session.terminal = Some(ObserverError::Application),
                3 => session.closed.set(true),
                4 => session.report.borrow_mut().local_stop = Some(LocalStop::FocusLost),
                5 => session.report.borrow_mut().close_exchange = None,
                6 => session.report.borrow_mut().attempts = 32,
                _ => unreachable!(),
            }
            assert_eq!(session.completion_action(), Err(CallbackError));
            assert_eq!(session.report().display_changes, 0);
            assert!(session.report().display_change_closes.is_empty());
        }
    }

    #[test]
    fn only_successful_requested_terminal_reports_can_accompany_a_reopen() {
        use frd::session_startup::{Error, StreamingViewerError};
        let report = fr_wire::closure::Closed {
            reason: fr_wire::closure::ClosedReason::ClientRequested,
            cleanup: fr_wire::closure::Cleanup::Complete,
            effects: fr_wire::closure::OutstandingEffects::Unknown,
        };
        let error = |report| {
            ObserverError::Streaming(StreamingViewerError::Session(Error::RemoteClosed(report)))
        };
        let mut session = selected_close();
        session.terminal = Some(error(report));
        assert_eq!(session.terminal_failure(), None);
        session.service_succeeded = false;
        assert_eq!(session.terminal_failure(), Some(error(report)));
        session.service_succeeded = true;
        let adverse = fr_wire::closure::Closed {
            cleanup: fr_wire::closure::Cleanup::Incomplete,
            ..report
        };
        session.terminal = Some(error(adverse));
        assert_eq!(session.terminal_failure(), Some(error(adverse)));
        assert_eq!(session.completion_action(), Err(CallbackError));
    }

    #[test]
    fn unavailable_file_and_clipboard_capabilities_only_omit_from_the_original_offer() {
        let mut session = failed_startup(false);
        let original = fr_client::native::control_offer_with_duplex(true, true);
        let filtered = session.offer(1, &original).unwrap();
        let mut expected = original.clone();
        expected.capabilities.retain(|cap| {
            !matches!(
                cap.name.as_str(),
                fr_wire::attachment::FILES_CAPABILITY
                    | fr_wire::files::CAPABILITY
                    | fr_wire::files::CHANNEL_SCOPE_CAPABILITY
                    | fr_wire::files::DOWNLOAD_CAPABILITY
                    | fr_wire::files::DUPLEX_CAPABILITY
                    | fr_wire::attachment::CLIPBOARD_CAPABILITY
                    | fr_wire::clipboard::CAPABILITY
                    | fr_wire::clipboard::startup::CAPABILITY
            )
        });
        assert_eq!(filtered, expected);
        assert_eq!(session.offer(2, &original).unwrap(), expected);

        let mut required = original;
        required
            .capabilities
            .iter_mut()
            .find(|cap| cap.name == fr_wire::files::CAPABILITY)
            .unwrap()
            .required = true;
        assert_eq!(session.offer(3, &required), Err(CallbackError));
    }

    #[test]
    fn terminal_batch_fences_ignore_timestamps_and_keep_close_escape_priority() {
        let mut events = vec![
            WindowEvent::Key {
                usage: 4,
                pressed: true,
                repeat: false,
                captured: Some(Duration::from_millis(1)),
                modifiers: 0,
            },
            WindowEvent::Overflow,
            WindowEvent::FocusLost,
            WindowEvent::Key {
                usage: 0x29,
                pressed: true,
                repeat: true,
                captured: None,
                modifiers: (1 << 17) | (1 << 18) | (1 << 19),
            },
            WindowEvent::CloseRequested,
        ];
        assert_eq!(terminal_event(&events), Some(LocalStop::WindowClosed));
        events.pop();
        assert_eq!(terminal_event(&events), Some(LocalStop::LocalEscape));
        events.pop();
        assert_eq!(terminal_event(&events), Some(LocalStop::FocusLost));
        events.pop();
        assert_eq!(terminal_event(&events), Some(LocalStop::Overflow));
        events.pop();
        assert_eq!(terminal_event(&events), None);
    }

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
