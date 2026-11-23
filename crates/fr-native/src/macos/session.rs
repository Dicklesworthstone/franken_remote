//! The macOS client session (fr-5a4w). frd's reconnecting supervisor calls
//! [`Session`] once per authenticated attempt; each attempt gets its own
//! window, opened only after the host's display was selected and sized to it,
//! and its own in-process `VideoToolbox` decoder drawing through Metal.
//!
//! With control, the session confirms the window's mapping, reports frames as
//! visible only while the window is actually on screen, requests control once
//! the session reports current evidence, and after the grant hands `AppKit`
//! input to the granted session's one event `Source`. An app switch releases
//! held keys/buttons and pauses input while the original clipboard's separate
//! Off surface stays live. Hiding, a changed picture size, event overflow or a
//! closed window stop that source (frd's contract for a native producer):
//! control then ends with the session.
//! Checked native geometry changes can reopen a fresh viewing window after
//! original cleanup, retaining only local point size/position. They never reuse
//! the old mapping, control, clipboard, file selection or text composition.
//! Stop control uses the same orderly close and confirmed cleanup as Change
//! Display, then opens a fresh viewing attempt with a new catalog selection.
//! The invocation's initial control choice is consumed once. A recovered
//! window stays viewing until a fresh F8 press and release requests a new
//! grant; Ctrl+Alt+Shift+Escape closes the session locally.
//!
//! Main thread only: `AppKit` owns the window and its event loop, which this
//! session services between network turns of the same runtime.
use super::{
    AUDIO_FLAG, HevcDecoder, PicturePoint, STOP_CONTROL_FLAG, SYSTEM_SHORTCUTS_FLAG, Surface,
    TEXT_COMPOSER_FLAG, TextCommit, TextRetireMode, TextRetirement, TextTaken, Window, WindowEvent,
    clipboard, event_time, files, picker, sharing,
};
use crate::NativeError;
use crate::macos_control::{Intent, Key, Route};
use crate::macos_live_observation;
use crate::macos_observe_once::{self, Once};
use crate::macos_resize::{Intent as ResizeIntent, Placement};
use crate::macos_scroll::{CaptureClock, Refusal as ScrollRefusal, Scroll};
use crate::macos_switch::{Action as ReopenAction, Intent as ReopenIntent, Mode as ReopenMode};
use asupersync::{
    cx::Cx,
    time::{Sleep, sleep},
};
use fr_client::input::{ClientInstant, viewport::SurfaceRect};
use fr_client::robot::live::{Evidence, Phase};
use fr_core::input::{DesktopPoint, InputBounds, KeyTransition, PhysicalKey, PointerButton};
use fr_core::input_submission::{Capabilities, Capability};
use fr_media::{
    access_unit::EncodedAccessUnit,
    freshness::{ClockPolicy, Error as FreshnessError},
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
    native_connection::reconnect::{
        self, Application, CallbackError, CompletionAction, LocalLayoutChange, Status,
    },
    session_startup::{
        ControlledViewer, ControlledViewerError, InteractiveViewerState, NativeObserver,
        ObserverError, ObserverPolicy, Presentation, StreamingViewerControl, StreamingViewerError,
        Viewer,
        viewer_events::{
            Error as CaptureError, Event, InputContext, Layout, LocalPoint, Located,
            MAX_EVENT_AGE_US, NativeCapture, PositionedAction, Source, TextContext,
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
    time::{Duration, Instant},
};

/// How often `AppKit` is serviced while the network is idle.
const PUMP: Duration = Duration::from_millis(4);
pub use super::clipboard::Report as ClipboardReport;
pub use crate::macos_observe_once::InitialObservation;
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
    SystemShortcutsUnavailable,
    SystemShortcutsStopped,
    SystemShortcutsOff,
}

/// Content-free tallies for the CLI's completion line.
#[derive(Debug, Default, Clone)]
pub struct Report {
    pub clipboard: ClipboardReport,
    pub microphone: Option<MicrophoneReport>,
    pub attempts: u8,
    pub opened: u32,
    pub presented: u64,
    /// Gaps between consecutive local submissions within each attempt.
    pub frame_intervals: super::cadence::FrameCadence,
    /// The last attempt's decoder, recovery and presented-age statistics,
    /// sampled before its observer closes.
    pub media: Option<frd::session_startup::ViewerStatistics>,
    pub hardware_decode: Option<bool>,
    pub approval_requested: bool,
    pub control_requested: bool,
    pub control_granted: bool,
    pub control_requests: u32,
    pub viewing_reconnects: u32,
    pub display_changes: u32,
    pub display_change_closes: Vec<DisplayChangeReport>,
    pub control_releases: u32,
    /// Times a grant paused because the window lost focus or was covered.
    /// Each pause released every held key and button on the host.
    pub control_pauses: u32,
    /// Original close evidence from explicit Stop control actions, retained
    /// separately from the subsequent viewing attempts and their results.
    pub control_release_closes: Vec<DisplayChangeReport>,
    pub window_resizes: u32,
    /// Failed original attempts, retained separately from a successful reopen.
    pub window_resize_closes: Vec<WindowResizeReport>,
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

/// Original terminal evidence retained before opening a fresh viewing attempt.
#[derive(Debug, Clone, Copy)]
pub struct DisplayChangeReport {
    pub attempt: u8,
    pub close_exchange: Option<fr_transport::quic::CloseOutcome>,
    pub lease_revocation: Option<fr_wire::lease_revoked::Revoked>,
}

#[derive(Debug, Clone, Copy)]
pub struct WindowResizeReport {
    pub original: DisplayChangeReport,
    pub interruption: reconnect::Failure,
}

type Choose = Box<dyn FnMut(&Catalog) -> Result<Option<u128>, ()>>;

/// The one producer frd owns for a granted session. `stop` is lock-free;
/// `try_reap` reports done once the event source has been dropped.
struct Capture {
    stop: Arc<AtomicBool>,
    released: Arc<AtomicBool>,
    shortcuts: Option<super::ShortcutRetirement>,
}
impl NativeCapture for Capture {
    fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
    fn try_reap(&mut self) -> bool {
        self.released.load(Ordering::SeqCst)
            && self
                .shortcuts
                .as_ref()
                .is_none_or(super::ShortcutRetirement::is_complete)
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
    /// Original ticket/view deadline, independently of optional text support.
    capture_until: u64,
    shortcuts: Option<super::ShortcutRetirement>,
    shortcut_context: Option<InputContext>,
    shortcut_serial: u32,
    shortcut_local_failure: Cell<bool>,
    /// The window lost focus or was covered: everything held was released and
    /// nothing is delivered until it is key and visible again.
    paused: bool,
    /// The host location of the latest positioned event, where a pause
    /// releases held buttons.
    last: Option<Located>,
}
impl Drop for Input {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
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
    fn refresh_capture(&mut self, viewer: &mut ControlledViewer) -> Result<(), ()> {
        if viewer.input_result_binding() != self.binding {
            return Err(());
        }
        let until = viewer
            .input_capture_context()
            .ok()
            .map(|(_, until)| until.0);
        self.capture_until = until.unwrap_or(0);
        if self.shortcuts.is_some() {
            let context = self.source.input_context().map_err(|_| ())?;
            if context != self.shortcut_context {
                // A release may have remained local while native capture was
                // paused. Do not carry held transitions into a resumed view.
                if self.keys.iter().any(|held| *held) || self.buttons.iter().any(|held| *held) {
                    self.shortcut_local_failure.set(true);
                    self.source.stop();
                    self.stop.store(true, Ordering::SeqCst);
                    return Err(());
                }
                self.shortcut_serial = self.shortcut_serial.checked_add(1).ok_or(())?;
                self.shortcut_context = context;
            }
        }
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

    /// Sample native time before the source clock. Adding only the remaining
    /// original ticket lifetime cannot extend it while crossing clock domains.
    /// The native owner separately caps each renewal to its pump watchdog.
    fn shortcut_snapshot(&self) -> Option<(u32, u64, Duration)> {
        if self.shortcuts.is_none() || self.paused || self.stopped() {
            return None;
        }
        let native = event_time();
        let client = self.source.clock().ok().map(|now| now.0);
        let ready = self
            .shortcut_context
            .as_ref()
            .is_some_and(|context| self.source.input_context_ready(context).unwrap_or(false));
        let until = ready
            .then(|| crate::macos_shortcuts::native_deadline(native, client, self.capture_until))
            .flatten();
        let Some(until) = until else {
            // Even a same-context compositor gap may keep key-up local while
            // capture is paused. The original lease cleanup must release held
            // state before another view can resume, never guess at that key-up.
            if self.keys.iter().any(|held| *held) || self.buttons.iter().any(|held| *held) {
                self.shortcut_local_failure.set(true);
                self.stop.store(true, Ordering::SeqCst);
                self.source.stop();
            }
            return None;
        };
        Some((self.shortcut_serial, self.capture_until, until))
    }

    fn shortcut_stop(&self) -> Option<LocalStop> {
        if self.shortcut_local_failure.get() {
            return Some(LocalStop::SystemShortcutsStopped);
        }
        self.shortcuts.as_ref()?.reason().map(|reason| {
            if reason == crate::macos_shortcuts::Stop::DisabledByUser {
                LocalStop::SystemShortcutsOff
            } else {
                LocalStop::SystemShortcutsStopped
            }
        })
    }

    /// Keep native composition attached to its original source and uninterrupted
    /// view. A compositor gap disables Send without cancelling a valid draft.
    fn text_status(&mut self) -> (Option<u64>, u64, bool) {
        if self.paused {
            // The native draft was discarded at the pause. The same context
            // returns on resume, so a new draft needs no new context.
            return (None, 0, false);
        }
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
            if self.paused
                || self.text_serial != Some(commit.context)
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
        if self.paused {
            return Ok(());
        }
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
        let original_shortcut = self.shortcut_admission(event, sampled, now)?;
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
                let location = self.layout.at(self.local(at));
                self.last = Some(location.clone());
                Event::Pointer(location)
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
                let location = self.layout.at(self.local(at));
                self.last = Some(location.clone());
                Event::Positioned {
                    location,
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
        if let Some((context, until)) = original_shortcut {
            self.source
                .push_in_context_until(&context, event, sampled, until)
                .map_err(|_| ())
        } else {
            self.source.push(event, sampled).map_err(|_| ())
        }
    }

    /// Recover only the snapshot published before this native key callback.
    /// Missing or stale metadata never falls back to ordinary key admission.
    fn shortcut_admission(
        &self,
        event: &WindowEvent,
        sampled: ClientInstant,
        now: ClientInstant,
    ) -> Result<Option<(InputContext, ClientInstant)>, ()> {
        let WindowEvent::Key { shortcut, .. } = event else {
            return Ok(None);
        };
        match (self.shortcuts.is_some(), shortcut) {
            (true, Some(original)) => {
                let until = crate::macos_shortcuts::captured_deadline(
                    original.context,
                    self.shortcut_serial,
                    original.until,
                    sampled.0,
                    now.0,
                    MAX_EVENT_AGE_US,
                )
                .ok_or(())?;
                let context = self.shortcut_context.clone().ok_or(())?;
                Ok(Some((context, ClientInstant(until))))
            }
            (false, None) => Ok(None),
            _ => Err(()),
        }
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
        let location = self.layout.at(self.local(at));
        self.last = Some(location.clone());
        let event = Event::Positioned {
            location,
            action: PositionedAction::Scroll {
                x: delta.x,
                y: delta.y,
                unit: delta.unit,
            },
        };
        Some((event, ClientInstant(delta.sampled)))
    }

    /// Release every key, then every button, this owner holds on the host, and
    /// deliver nothing more until [`Input::resume`]. Nothing is restored on
    /// resume. Err: a release was refused, so control must end.
    fn pause(&mut self, report: &mut Report) -> Result<(), ()> {
        if self.paused {
            return Ok(());
        }
        self.paused = true;
        report.control_pauses = report.control_pauses.saturating_add(1);
        self.scroll.reset();
        let sampled = self.source.clock().map_err(|_| ())?;
        for usage in 0..=u8::MAX {
            if !std::mem::take(&mut self.keys[usize::from(usage)]) {
                continue;
            }
            // `deliver` holds only keys `PhysicalKey` accepts.
            let key = PhysicalKey::new(u16::from(usage)).ok_or(())?;
            let release = Event::Key {
                key,
                transition: KeyTransition::Release,
            };
            self.source.push(release, sampled).map_err(|_| ())?;
        }
        for (index, button) in HELD_BUTTONS.into_iter().enumerate() {
            if !std::mem::take(&mut self.buttons[index]) {
                continue;
            }
            // Every press carried a location, so a held button has one.
            let location = self.last.clone().ok_or(())?;
            let release = Event::Positioned {
                location,
                action: PositionedAction::Button {
                    button,
                    pressed: false,
                },
            };
            self.source.push(release, sampled).map_err(|_| ())?;
        }
        Ok(())
    }

    /// The window is key and visible again: input flows from the next turn.
    const fn resume(&mut self) {
        self.paused = false;
    }
}

/// `Input::buttons` slots, in `deliver`'s order.
const HELD_BUTTONS: [PointerButton; 5] = [
    PointerButton::Primary,
    PointerButton::Secondary,
    PointerButton::Middle,
    PointerButton::Back,
    PointerButton::Forward,
];

/// What a granted owner does about focus this turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FocusStep {
    Continue,
    Pause,
    Resume,
}

/// `away`: the window is not key or not visible now. `pause_event`: this
/// batch reported a pausing focus loss. An app switch and back within one poll
/// still pauses, because native forgot the keys it held and will not report
/// their releases. Pause and resume each discard their turn's batch: those
/// events may predate the change.
const fn focus_step(paused: bool, away: bool, pause_event: bool) -> FocusStep {
    if away || (pause_event && !paused) {
        FocusStep::Pause
    } else if paused {
        FocusStep::Resume
    } else {
        FocusStep::Continue
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
    fn read_presentation(
        &mut self,
        expected: fr_media::worker::presentation_snapshot::Stamp,
        maximum_body_bytes: usize,
    ) -> Result<
        Result<
            fr_media::worker::presentation_snapshot::Snapshot,
            fr_media::worker::presentation_snapshot::Absent,
        >,
        WorkerError,
    > {
        self.decoder
            .as_mut()
            .ok_or(WorkerError::WrongState)?
            .read_presentation(expected, maximum_body_bytes)
            .map_err(worker_error)
    }
}

/// This attempt's window, kept until mandatory cleanup. The pump and the
/// UI callbacks take only short borrows of it.
type WindowSlot = Rc<RefCell<Option<Window>>>;

/// Independently retained original audio cleanup. A stop only fences admission;
/// completion requires the actual device, decoder child and native worker to
/// retire. Implementations must remain nonblocking on the `AppKit` thread.
pub trait AudioRetirement {
    fn stop(&self);
    fn is_complete(&self) -> bool;
    fn view(&self) -> AudioView;
    fn preferences(&self, volume_percent: u8, muted: bool);
    fn controls_failed(&self);
}

/// Content-free native playback status. Playing means actual PCM submission,
/// never independently measured speaker audibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum AudioState {
    Starting = 0,
    Playing = 1,
    Unavailable = 2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioView {
    pub state: AudioState,
    pub volume_percent: u8,
    pub muted: bool,
}

type InstallAudio = Box<dyn FnMut(&mut NativeObserver) -> Box<dyn AudioRetirement>>;
type AudioSlot = Rc<RefCell<Option<Rc<dyn AudioRetirement>>>>;

/// Original client microphone owner. Native/UI operations remain nonblocking
/// on the session thread; completion includes actual permission/capture reap.
pub trait Microphone {
    fn may_offer(&self) -> bool;
    fn turn(&mut self, viewer: &mut ControlledViewer) -> Result<(), CallbackError>;
    fn pump(&mut self, visible: bool, stopped: bool);
    fn stop(&mut self);
    fn complete(&mut self) -> bool;
    fn report(&self) -> MicrophoneReport;
}

#[derive(Debug, Clone)]
// Independent observations: request, host selection, TCC grant and source ready.
#[allow(clippy::struct_excessive_bools)]
pub struct MicrophoneReport {
    pub requested: bool,
    pub selected: bool,
    pub permission_granted: bool,
    /// Actual source Ready reply, before any proof of PCM or remote audibility.
    pub capture_ready: bool,
    pub state: &'static str,
    pub reason: Option<&'static str>,
}
impl Default for MicrophoneReport {
    fn default() -> Self {
        Self {
            requested: false,
            selected: false,
            permission_granted: false,
            capture_ready: false,
            state: "not_started",
            reason: None,
        }
    }
}
type MicrophoneSlot = Rc<RefCell<Option<Box<dyn Microphone>>>>;

/// One explicitly enabled local robot producer on this foreground connection.
/// The authenticated local owner keeps the action ledger and original view
/// preconditions. The session supplies only its own receipts and granted
/// controller; native keyboard/mouse forwarding is never attached alongside it.
pub trait Automation {
    fn ready(
        &self,
        snapshot: fr_client::robot::snapshot::Handle,
        display: Display,
        initial: Presentation,
        received_at: Instant,
    ) -> Result<(), CallbackError>;
    fn presented(&self, frame: Presentation) -> Result<(), CallbackError>;
    fn turn(&self, viewer: &mut ControlledViewer);
    fn result(&self, result: fr_client::input::ResultEvent);
    /// Fence before entering native cleanup; retained effects remain queryable
    /// through the original local owner until its ordinary bounded retirement.
    fn retire(&self);
}

// These facts have independent owners: chooser configuration, actual service
// completion, startup ownership and positively completed cleanup.
#[allow(clippy::struct_excessive_bools)]
pub struct Session {
    control: Option<ControlPolicy>,
    /// Explicit invocation consent is spent by the first capture attachment.
    /// Recovery and later F8 requests never reinstall a retired event tap.
    system_shortcuts: Rc<Cell<bool>>,
    policy: ObserverPolicy,
    choose: Choose,
    display_picker: bool,
    picker: Rc<RefCell<Option<picker::DisplayPicker>>>,
    picker_error: Rc<Cell<Option<picker::Error>>>,
    close_cx: Option<Cx>,
    epoch: u128,
    title: String,
    fit_window: Option<(u32, u32)>,
    /// The screen's 1:1 box: each attempt states the part its window (the
    /// fit ceiling, then each resized window) shows, and a scaled picture's
    /// window opens at picture pixels / scale.
    picture_box: Option<crate::macos::WindowBox>,
    placement: Option<Placement>,
    resize: Rc<ResizeIntent>,
    window: WindowSlot,
    /// The started observer, reaped in `cleanup`. `run` and `cleanup` both
    /// take `&mut self`, so `run`'s borrow has ended before cleanup starts.
    observer: Option<NativeObserver>,
    observations: Option<macos_live_observation::Publisher>,
    once: Option<Once>,
    automation: Option<Rc<dyn Automation>>,
    input: Rc<RefCell<Option<Input>>>,
    files: Rc<RefCell<files::Files>>,
    clipboard: Rc<RefCell<clipboard::Clipboard>>,
    audio_install: Option<InstallAudio>,
    audio_owner: AudioSlot,
    microphone: MicrophoneSlot,
    report: Rc<RefCell<Report>>,
    closed: Rc<Cell<bool>>,
    intent: Rc<RefCell<Intent>>,
    reopen_intent: Rc<RefCell<ReopenIntent>>,
    service_succeeded: bool,
    service_failure: Option<ObserverError>,
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
        if let Some(observations) = &self.observations {
            let _ = observations.retire(Phase::Stopped);
        }
        if let Some(automation) = &self.automation {
            automation.retire();
        }
        if let Some(observer) = &self.observer {
            observer.control().stop();
        }
        if let Some(audio) = self.audio_owner.borrow().as_ref() {
            audio.stop();
        }
        if let Some(microphone) = self.microphone.borrow_mut().as_mut() {
            microphone.stop();
        }
        if let Ok(mut input) = self.input.try_borrow_mut() {
            input.take();
        }
        if let Ok(window) = self.window.try_borrow()
            && let Some(window) = window.as_ref()
        {
            let _ = window.stop_system_shortcuts();
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
            system_shortcuts: Rc::default(),
            policy,
            choose,
            display_picker: false,
            picker: Rc::default(),
            picker_error: Rc::default(),
            close_cx: None,
            epoch,
            title,
            fit_window: None,
            picture_box: None,
            placement: None,
            resize: Rc::default(),
            window: Rc::default(),
            observer: None,
            observations: None,
            once: None,
            automation: None,
            input: Rc::default(),
            files: Rc::default(),
            clipboard: Rc::default(),
            audio_install: None,
            audio_owner: Rc::default(),
            microphone: Rc::default(),
            report: Rc::default(),
            closed: Rc::default(),
            intent: Rc::new(RefCell::new(Intent::new(control.is_some()))),
            reopen_intent: Rc::default(),
            service_succeeded: false,
            service_failure: None,
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

    /// Enable system-shortcut capture for one original native control owner.
    /// Accessibility permission and a real modifying event tap are still
    /// required at attachment; this preference grants no remote authority.
    #[must_use]
    pub fn with_system_shortcuts(self, enabled: bool) -> Self {
        self.system_shortcuts.set(enabled && self.control.is_some());
        self
    }
    /// Observe exactly one original native receipt, optionally export its
    /// viewport, then request `InspectionComplete` and perform ordinary cleanup.
    /// This finite mode cannot acquire control, auxiliary lanes, a publisher,
    /// display selection UI, or a second authenticated attempt.
    pub fn observing_once(mut self, screenshot: bool) -> Result<Self, CallbackError> {
        if self.once.is_some()
            || !self.one_shot_profile()
            || self.started
            || self.native_started.get()
            || self.observer.is_some()
            || self.report.borrow().attempts != 0
        {
            return Err(CallbackError);
        }
        self.once = Some(Once::new(screenshot));
        Ok(self)
    }
    /// Consume the original historical observation only after its successful
    /// close operation and confirmed native cleanup. No receipt time is renewed
    /// and no image is reacquired. A refused snapshot remains explicit absence.
    pub fn take_initial_observation(&mut self) -> Option<InitialObservation> {
        let succeeded = self.service_succeeded
            && self.terminal_failure().is_none()
            && !self.closed.get()
            && self.report.borrow().local_stop.is_none();
        self.once.as_mut()?.take(succeeded, self.cleanup_confirmed)
    }
    fn one_shot_profile(&self) -> bool {
        self.control.is_none()
            && !self.system_shortcuts.get()
            && self.observations.is_none()
            && self.automation.is_none()
            && !self.display_picker
            && self.audio_install.is_none()
            && self.microphone.borrow().is_none()
            && !self.files.borrow().has_upload()
            && !self.files.borrow().has_download()
            && !self.clipboard.borrow().available()
    }
    /// Explicit read-only observation publication from this original invocation.
    /// Install before any attempt. A control-capable session cannot acquire
    /// this view-only interface. The sink must bind the original snapshot
    /// handle explicitly; no pixels are exported until an authenticated read.
    pub fn with_observations(
        mut self,
        publish: impl FnMut(macos_live_observation::Event) -> Result<(), ()> + 'static,
    ) -> Result<Self, CallbackError> {
        if self.control.is_some()
            || self.once.is_some()
            || self.automation.is_some()
            || self.started
            || self.native_started.get()
            || self.observer.is_some()
            || self.observations.is_some()
            || self.report.borrow().attempts != 0
        {
            return Err(CallbackError);
        }
        self.observations = Some(macos_live_observation::Publisher::new(publish));
        Ok(self)
    }
    /// Exclusive foreground robot control, installed before the single original
    /// attempt. No auxiliary consent, display chooser, publisher or finite
    /// observation can share this producer. Callers still negotiate ordinary
    /// host control and must preserve its exact authorization/preconditions.
    pub fn with_automation(
        mut self,
        automation: impl Automation + 'static,
    ) -> Result<Self, CallbackError> {
        if !self.automation_profile()
            || self.automation.is_some()
            || self.started
            || self.native_started.get()
            || self.observer.is_some()
            || self.report.borrow().attempts != 0
        {
            return Err(CallbackError);
        }
        self.automation = Some(Rc::new(automation));
        Ok(self)
    }
    fn automation_profile(&self) -> bool {
        self.control.is_some()
            && !self.system_shortcuts.get()
            && self.once.is_none()
            && self.observations.is_none()
            && !self.display_picker
            && self.audio_install.is_none()
            && self.microphone.borrow().is_none()
            && !self.files.borrow().has_upload()
            && !self.files.borrow().has_download()
            && !self.clipboard.borrow().available()
    }
    /// Supply an explicitly requested local output for each fresh original
    /// observer, before serving it. The installer handles an unselected audio
    /// capability as typed absence. Its independent cleanup owner is fenced
    /// before any native window teardown and must finish before another attempt.
    #[must_use]
    pub fn with_audio_output(
        mut self,
        install: impl FnMut(&mut NativeObserver) -> Box<dyn AudioRetirement> + 'static,
    ) -> Self {
        self.audio_install = Some(Box::new(install));
        self
    }
    /// One explicit microphone choice for this invocation's first controller.
    /// Recovered viewing and subsequent control grants cannot reuse its intent.
    #[must_use]
    pub fn with_microphone(self, microphone: impl Microphone + 'static) -> Self {
        self.report.borrow_mut().microphone = Some(microphone.report());
        *self.microphone.borrow_mut() = Some(Box::new(microphone));
        self
    }
    /// An optional logical point bound for the fitted viewer content.
    #[must_use]
    pub fn with_fit_window(mut self, fit: Option<(u32, u32)>) -> Self {
        self.fit_window = fit;
        self
    }
    /// The screen's 1:1 box (`crate::macos::window_box`). Each attempt
    /// states the part of it the window shows (`scaled-picture`), so a
    /// scaling host sends a picture drawn 1:1, also after a resize.
    #[must_use]
    pub fn with_picture_box(mut self, picture_box: Option<crate::macos::WindowBox>) -> Self {
        self.picture_box = picture_box;
        self
    }
    /// The box the next attempt states: the window's part of the screen's.
    #[must_use]
    pub fn stated_picture(&self) -> Option<(u32, u32)> {
        let full_screen = self
            .placement
            .is_some_and(|placement| placement.full_screen);
        self.picture_box?.within(self.fit_window, full_screen)
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
    /// The original control offer with the optional precise-scroll and
    /// committed-text capabilities this session's policy requests. They must
    /// be in the ORIGINAL: each attempt's [`Application::offer`] may only
    /// remove from it, and the reconnect loop refuses a selection holding any
    /// capability the original lacks (that refusal ended every macOS control
    /// attempt as `Notification` while they were added per attempt).
    /// Observation, or an offer that does not request control, is unchanged.
    pub fn control_offer(&self, offer: Offer) -> Result<Offer, fr_wire::negotiation::Error> {
        let Some(policy) = self.control else {
            return Ok(offer);
        };
        if offer.role != fr_wire::negotiation::Role::RequestControl {
            return Ok(offer);
        }
        let offer = if policy.capabilities.contains(Capability::PixelScroll) {
            fr_wire::control::with_pixel_scroll(offer)?
        } else {
            offer
        };
        if policy.capabilities.contains(Capability::Text) {
            fr_wire::control::with_committed_text(offer)
        } else {
            Ok(offer)
        }
    }
    /// Select the original foreground owner's comparison identity before file
    /// setup. An already configured or cancelled endpoint cannot be rebound.
    pub fn file_command_connection(
        &mut self,
        connection: &str,
    ) -> Result<(), frd::native_files::local::Error> {
        self.files.borrow_mut().command_connection(connection)
    }
    /// Fence first, then inspect the original window's final bounded native
    /// turn. Ordinary events are discarded; no input or text is replayed.
    fn seal_window_resize(&self) {
        if !self.resize.armed() {
            return;
        }
        let mut events = Vec::with_capacity(64);
        let mut window = self.window.borrow_mut();
        let Some(opened) = window.as_mut() else {
            self.resize.cancel();
            return;
        };
        opened.poll(Duration::ZERO, &mut events);
        let mut terminal = terminal_event(&events);
        // A full-screen transition fenced the original at its start; its
        // final size exists only once AppKit has finished animating. Pump the
        // window (events discarded, terminal ones kept) for at most this long.
        let settling = std::time::Instant::now();
        while terminal.is_none()
            && self.resize.changed()
            && opened.full_screen_moving().unwrap_or(false)
            && settling.elapsed() < FULL_SCREEN_SETTLE
        {
            events.clear();
            opened.poll(Duration::from_millis(10), &mut events);
            terminal = terminal_event(&events);
        }
        if let Some(reason) = terminal {
            self.resize.cancel();
            let mut report = self.report.borrow_mut();
            if report
                .local_stop
                .is_none_or(|reason| reason == LocalStop::PictureChanged)
            {
                report.local_stop = Some(reason);
            }
            if matches!(reason, LocalStop::WindowClosed | LocalStop::LocalEscape) {
                self.closed.set(true);
            }
        } else if self.resize.changed() {
            self.intent.borrow_mut().retire_initial();
            self.reopen_intent.borrow_mut().cancel();
            self.report
                .borrow_mut()
                .local_stop
                .get_or_insert(LocalStop::PictureChanged);
            match opened.seal_resize() {
                Ok(sample) => {
                    self.resize.seal(sample);
                }
                Err(_) => self.resize.cancel(),
            }
        }
    }

    fn window_resize_action(
        &mut self,
        failure: reconnect::Failure,
        now: Option<Duration>,
    ) -> Option<LocalLayoutChange> {
        if !self.resize.changed() {
            return None;
        }
        let report = self.report.borrow();
        if self.service_succeeded
            || !self.cleanup_confirmed
            || self.started
            || self.native_started.get()
            || self.observer.is_some()
            || self.picker.borrow().is_some()
            || self.closed.get()
            || report.local_stop != Some(LocalStop::PictureChanged)
            || self.reopen_intent.borrow().pending()
            || self.terminal_failure().is_some()
            || !(1..32).contains(&report.attempts)
            || report.window_resize_closes.len() >= 31
            || !reconnect::resize_interrupted(failure)
            || self.service_failure.is_some_and(|error| {
                matches!(
                    error,
                    ObserverError::Application
                        | ObserverError::Streaming(
                            frd::session_startup::StreamingViewerError::Application
                        )
                ) || !reconnect::resize_interrupted(reconnect::Failure::Observation(error))
            })
        {
            self.resize.cancel();
            return None;
        }
        let original = DisplayChangeReport {
            attempt: report.attempts,
            close_exchange: report.close_exchange,
            lease_revocation: report.lease_revocation,
        };
        drop(report);
        let placement = self.resize.take(now)?;
        self.fit_window = Some(placement.size);
        self.placement = Some(placement);
        self.intent.borrow_mut().retire_initial();
        self.files.borrow_mut().cancel();
        let mut report = self.report.borrow_mut();
        report.window_resizes += 1;
        report.window_resize_closes.push(WindowResizeReport {
            original,
            interruption: failure,
        });
        // Only this precise checked interruption loses its local stop latch;
        // neither focus/hide/close nor another native failure can enter here.
        report.local_stop = None;
        Some(LocalLayoutChange::WindowResize)
    }

    /// Original-attempt receipts and cumulative local activity.
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

async fn reap_audio(owner: Option<&dyn AudioRetirement>, cx: &Cx, deadline: Deadline) -> bool {
    let Some(owner) = owner else {
        return true;
    };
    loop {
        if owner.is_complete() {
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

/// Consume local preferences before refreshing labels, always against this
/// window's original output handle. A failed optional control fences audio
/// without inventing a failure for the still-working desktop.
fn service_audio(window: &RefCell<Option<Window>>, audio: &AudioSlot, stopped: bool) {
    let owner = audio.borrow().clone();
    let Some(owner) = owner else {
        return;
    };
    if stopped {
        owner.stop();
        return;
    }
    let window = window.borrow();
    let Some(window) = window.as_ref() else {
        return;
    };
    let result = (|| {
        if let Some((volume, muted)) = window.audio_preferences()? {
            owner.preferences(volume, muted);
        }
        window.audio_update(owner.view())
    })();
    if result.is_err() {
        owner.controls_failed();
    }
}

fn service_microphone(
    window: &WindowSlot,
    microphone: &MicrophoneSlot,
    report: &Rc<RefCell<Report>>,
    stopped: bool,
) {
    let mut owner = microphone.borrow_mut();
    let Some(owner) = owner.as_mut() else { return };
    let visible = window
        .borrow()
        .as_ref()
        .and_then(|window| window.state().ok())
        .is_some_and(|state| state.visible);
    owner.pump(visible, stopped);
    report.borrow_mut().microphone = Some(owner.report());
}

async fn reap_microphone(owner: &MicrophoneSlot, cx: &Cx, deadline: Deadline) -> bool {
    loop {
        if owner
            .borrow_mut()
            .as_mut()
            .is_none_or(|owner| owner.complete())
        {
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

/// Input is fenced before removing the original native callback. Retaining
/// this window until removal completes keeps capture cleanup independently
/// reportable; dropping a Source alone cannot acknowledge a live event tap.
fn retire_shortcuts(window: &RefCell<Option<Window>>) -> Result<(), ()> {
    window.borrow().as_ref().map_or(Ok(()), |opened| {
        opened.stop_system_shortcuts().map_err(|_| ())
    })
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

fn producer_terminal_event(events: &[WindowEvent], automation: bool) -> Option<LocalStop> {
    terminal_event(events).or_else(|| {
        // Robot authority cannot survive an app switch, even when focus was
        // regained in this same batch. Ordinary native input still pauses.
        (automation && events.contains(&WindowEvent::FocusPaused)).then_some(LocalStop::FocusLost)
    })
}

/// The longest a resize seal waits for `AppKit` to finish a full-screen
/// animation (about 0.5-0.8 s), inside the resize receipt's 2 s bound.
const FULL_SCREEN_SETTLE: Duration = Duration::from_millis(1500);

fn window_stop(
    state: Option<super::WindowState>,
    input_picture: Option<(u32, u32)>,
    require_picture: bool,
) -> Option<LocalStop> {
    match state {
        None => Some(LocalStop::NativeFailure),
        Some(state) if !state.visible => Some(LocalStop::WindowHidden),
        Some(state) if !state.key => Some(LocalStop::FocusLost),
        Some(state)
            if (require_picture
                && (!state.geometry_current || state.picture.0 == 0 || state.picture.1 == 0))
                || input_picture.is_some_and(|picture| picture != state.picture) =>
        {
            Some(LocalStop::PictureChanged)
        }
        Some(_) => None,
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
    reopen_intent: &RefCell<ReopenIntent>,
    resize: &Rc<ResizeIntent>,
    can_resize: bool,
    automation: Option<&dyn Automation>,
    closed: &Cell<bool>,
    stop: &StreamingViewerControl,
    files: &RefCell<files::Files>,
    clipboard: &RefCell<clipboard::Clipboard>,
    sharing: &mut sharing::Ui,
    events: &mut Vec<WindowEvent>,
) {
    events.clear();
    let halt = |reason: LocalStop, input: &mut Option<Input>, report: &mut Report| {
        if let Some(automation) = automation {
            automation.retire();
        }
        report.local_stop.get_or_insert(reason);
        resize.cancel();
        reopen_intent.borrow_mut().cancel();
        *input = None;
        stop.stop();
        let _ = retire_shortcuts(window);
        let _ = retire_text(window, TextRetireMode::Fenced);
        files.borrow_mut().cancel();
        clipboard.borrow_mut().stop(window.borrow().as_ref());
    };
    let geometry_stopped = || {
        // Native already cancelled this exact original viewer before returning
        // from layout. Drop the producer before entering any native cleanup.
        if let Some(automation) = automation {
            automation.retire();
        }
        input.borrow_mut().take();
        stop.stop();
        let _ = retire_shortcuts(window);
        intent.borrow_mut().retire_initial();
        reopen_intent.borrow_mut().cancel();
        report
            .borrow_mut()
            .local_stop
            .get_or_insert(LocalStop::PictureChanged);
        files.borrow_mut().cancel();
        clipboard.borrow_mut().stop(window.borrow().as_ref());
    };
    if resize.changed() {
        geometry_stopped();
        return;
    }
    if let Some(automation) = automation
        && stop.is_stopped()
    {
        // The original controller may stop independently of the UI pump. Fence
        // the local action owner before any native call can delay retirement.
        automation.retire();
    }
    let retire_stopped = || -> Result<bool, ()> {
        let retired = {
            let mut slot = input.borrow_mut();
            if slot.as_ref().is_some_and(Input::stopped) {
                if let Some(reason) = slot.as_ref().and_then(Input::shortcut_stop) {
                    report.borrow_mut().local_stop.get_or_insert(reason);
                    if reason == LocalStop::SystemShortcutsOff {
                        closed.set(true);
                    }
                }
                *slot = None;
                true
            } else {
                false
            }
        };
        if !retired {
            return Ok(false);
        }
        if let Some(automation) = automation {
            automation.retire();
        }
        files.borrow_mut().cancel();
        intent.borrow_mut().invalidate();
        let pending = reopen_intent.borrow().pending();
        if !pending {
            stop.stop();
        }
        // A pending explicit close has already parked its original producer.
        let mode = if pending {
            TextRetireMode::Park
        } else {
            TextRetireMode::Fenced
        };
        retire_shortcuts(window)?;
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
        let text_updated = if reopen_intent.borrow().pending() {
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
        let shortcut_snapshot = if reopen_intent.borrow().pending() {
            None
        } else {
            input.borrow().as_ref().and_then(Input::shortcut_snapshot)
        };
        // No Source borrow or queue lock crosses the native boundary. An
        // unbound viewer stays unbound; this operation cannot install a tap.
        let (context, client_until, native_until) = shortcut_snapshot
            .map_or((0, 0, 0.0), |(context, until, native)| {
                (context, until, native.as_secs_f64())
            });
        let shortcuts_updated = opened
            .update_system_shortcuts(context, client_until, native_until)
            .is_ok();
        files.borrow().file_drop_ready(
            input
                .borrow()
                .as_ref()
                .is_some_and(|input| !input.stopped() && !input.paused),
        );
        if !shortcuts_updated {
            Some(LocalStop::SystemShortcutsStopped)
        } else if !text_updated
            || opened.file_selection_update().is_err()
            || opened
                .reopen_enabled(
                    reopen_intent.borrow().enabled(ReopenAction::ChangeDisplay),
                    reopen_intent.borrow().enabled(ReopenAction::StopControl),
                )
                .is_err()
        {
            Some(LocalStop::NativeFailure)
        } else {
            opened.poll(Duration::ZERO, events);
            // Native terminal callbacks defer IME cleanup. Fence owners before
            // state, clipboard or sharing can enter another native call.
            producer_terminal_event(events, automation.is_some())
        }
    };
    if terminal.is_none() && resize.changed() {
        geometry_stopped();
        return;
    }
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
                || automation.is_some()
                || input.is_some()
                || intent.requested()
                || reopen_intent.borrow().pending()
            {
                halt(reason, &mut input, &mut report);
            } else {
                // No input owner exists. Keep viewing, but discard old local
                // composition and any display choice across focus/queue loss.
                reopen_intent
                    .borrow_mut()
                    .set_mode(ReopenMode::Disabled, event_time());
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
    let reopen_action = opened.take_reopen();
    drop(slot);
    let reason = if reopen_action.is_err() {
        Some(LocalStop::NativeFailure)
    } else {
        // The opening pump runs before the renderer submits its first picture.
        // Its empty geometry is expected; it cannot authorize control yet.
        // After that actual receipt, stale geometry is permanently terminal.
        window_stop(
            state,
            input.as_ref().map(|input| input.picture),
            automation.is_none() || can_resize,
        )
    };
    // An app switch or a covered window pauses a grant instead of ending it.
    // Minimizing, hiding, an unavailable session and closing stay terminal
    // (`terminal_event`), as does a pending explicit close.
    let away = matches!(reason, Some(LocalStop::FocusLost | LocalStop::WindowHidden));
    let focus = match input.as_ref() {
        Some(granted) if (reason.is_none() || away) && !reopen_intent.borrow().pending() => {
            focus_step(
                granted.paused,
                away,
                events.contains(&WindowEvent::FocusPaused),
            )
        }
        _ => FocusStep::Continue,
    };
    match focus {
        FocusStep::Pause => {
            intent.suspend();
            let paused = input
                .as_mut()
                .is_some_and(|granted| granted.pause(&mut report).is_ok());
            if paused {
                // Release held input before querying AppKit. Clipboard owns
                // a separate visible Off panel; an app switch cannot starve
                // its original watchdog merely by pausing keyboard input.
                clipboard.borrow_mut().turn(window.borrow().as_ref());
            } else {
                intent.invalidate();
                halt(LocalStop::NativeFailure, &mut input, &mut report);
            }
            return;
        }
        FocusStep::Resume => {
            if let Some(granted) = input.as_mut() {
                granted.resume();
            }
            clipboard.borrow_mut().turn(window.borrow().as_ref());
            return;
        }
        FocusStep::Continue => {}
    }
    if let Some(reason) = reason {
        intent.suspend();
        if automation.is_some()
            || input.is_some()
            || intent.requested()
            || (reopen_intent.borrow().pending() && reason != LocalStop::PictureChanged)
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
            reopen_intent
                .borrow_mut()
                .set_mode(ReopenMode::Disabled, event_time());
            return;
        }
    }
    let now = event_time();
    if let Ok(Some((action, captured))) = reopen_action {
        let accepted = reopen_intent.borrow_mut().request(action, captured, now);
        if !accepted && action == ReopenAction::StopControl {
            // A stale/refused release gesture cannot authorize a fresh view,
            // but it must still stop the original control immediately.
            halt(LocalStop::NativeFailure, &mut input, &mut report);
            return;
        }
        if accepted {
            // Park this exact producer; dropping Source would cancel the original
            // session before its orderly close callback can fence the actual grant.
            // No new event enters it after this turn, including the click itself.
            if let Some(automation) = automation {
                automation.retire();
            }
            intent.retire_initial();
            if retire_text(window, TextRetireMode::Park).is_err() {
                halt(LocalStop::NativeFailure, &mut input, &mut report);
                return;
            }
            files.borrow_mut().cancel();
            // Native already retired its clipboard callback. Do not enter
            // chrome/IME work before the original service fences its grant.
            clipboard.borrow_mut().stop(None);
        }
    }
    let pending = reopen_intent.borrow().pending();
    let expired = reopen_intent.borrow().expired(now);
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
    if can_resize && !resize.armed() && state.is_some_and(super::WindowState::input_ready) {
        let watched = window
            .borrow()
            .as_ref()
            .is_some_and(|opened| opened.watch_resize(resize, stop).is_ok());
        if !watched {
            halt(LocalStop::NativeFailure, &mut input, &mut report);
            return;
        }
    }
    if let Some(automation) = automation {
        // Arming the watcher may itself enter AppKit and discover a terminal
        // geometry change. Fence before any further native work in this turn.
        if resize.changed() {
            halt(LocalStop::PictureChanged, &mut input, &mut report);
        } else if stop.is_stopped() {
            automation.retire();
        }
        // Escape, stop, focus and geometry handling above remain local. No
        // auxiliary UI or native input producer shares the robot controller.
        return;
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
    // Ordinary input still samples time after native chrome work; the earlier
    // handoff sample must not make a delayed key gesture appear current.
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
                ..
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
fn shown(
    window: &RefCell<Option<Window>>,
    pending: &mut Option<u64>,
    frame: Option<Presentation>,
) -> Option<u64> {
    let visible = window
        .borrow()
        .as_ref()
        .and_then(|w| w.state().ok())
        .is_some_and(|s| s.visible && s.geometry_current);
    take_shown(pending, frame, visible)
}

/// frd hands each later picture to the callback once, then calls again with
/// `None` after every network turn; the opening picture never comes through
/// the callback. Keep the newest submitted picture pending until the window is
/// visible, and report it once. A newer callback picture replaces it. An idle
/// host may send nothing newer, so without this the opening picture of a
/// static desktop would never count and control would never be requested.
fn take_shown(
    pending: &mut Option<u64>,
    frame: Option<Presentation>,
    visible: bool,
) -> Option<u64> {
    if let Some(frame) = frame {
        *pending = submitted(frame);
    }
    if !visible {
        return None;
    }
    pending.take()
}

/// frd's id for a picture it submitted for display. A decode-only picture is
/// never shown.
fn submitted(frame: Presentation) -> Option<u64> {
    (frame.stage == PresentationStage::SubmittedToCompositor).then(|| frame.frame.as_raw())
}

fn observation_evidence(stage: PresentationStage) -> Evidence {
    match stage {
        PresentationStage::DecodedOnly => Evidence::Decoded,
        PresentationStage::SubmittedToCompositor => Evidence::SubmittedToCompositor,
    }
}

/// Record the answer to a visibility report for `frame`, which [`shown`] has
/// just taken out of `pending`.
///
/// - The view exists only once the session clock is correlated. A picture
///   drawn while the session opened therefore meets `ClockNotReady` on the
///   first turns, so that refusal puts it back to report on a later turn.
/// - `QueueExpired` means the picture arrived on screen after its display
///   budget. frd still holds it as the visible picture, unqualified, until the
///   host verifies the source again, and its evidence gates input until then.
///   It counts as shown, so the view keeps checking that evidence; otherwise
///   a late opening picture on a static host would never offer control.
/// - Any other refusal waits for a newer picture, as in the Linux viewer.
fn witness<T>(
    result: &Result<T, StreamingViewerError>,
    frame: u64,
    pending: &mut Option<u64>,
    last_shown: &mut Option<u64>,
) {
    match result {
        Ok(_) | Err(StreamingViewerError::Freshness(FreshnessError::QueueExpired)) => {
            *last_shown = Some(frame);
        }
        Err(StreamingViewerError::Control(ControlledViewerError::ClockNotReady)) => {
            *pending = Some(frame);
        }
        Err(_) => {}
    }
}

/// Attach this window's input to the granted session with a layout that
/// maps the whole picture, as drawn now, onto the selected display.
fn attach(
    viewer: &mut ControlledViewer,
    display: Display,
    window: &RefCell<Option<Window>>,
    slot: &Rc<RefCell<Option<Input>>>,
    resize: &ResizeIntent,
    native_events: bool,
    system_shortcuts: &Cell<bool>,
) -> Result<bool, LocalStop> {
    let window = window.borrow();
    let window = window.as_ref().ok_or(LocalStop::NativeFailure)?;
    let state = window.state().map_err(|_| LocalStop::NativeFailure)?;
    let (width, height) = state.picture;
    if !state.input_ready() || !resize.watching() {
        return Ok(false); // wait for a current picture and its independent native fence
    }
    let source = InputBounds::new(
        DesktopPoint {
            x: display.x,
            y: display.y,
        },
        display.pixel_width,
        display.pixel_height,
    )
    .ok_or(LocalStop::NativeFailure)?;
    let area = SurfaceRect::new(0, 0, width, height).map_err(|_| LocalStop::NativeFailure)?;
    let layout = viewer
        .configure_viewport(source, area)
        .map_err(|_| LocalStop::NativeFailure)?;
    viewer
        .confirm_viewport(&layout)
        .map_err(|_| LocalStop::NativeFailure)?;
    if !native_events {
        // Robot input uses this exact displayed mapping directly; attaching a
        // second native Source would interleave unrelated held-state owners.
        return Ok(true);
    }
    // The layout is valid for this size only. The independently armed native
    // watcher fences it synchronously on the next real geometry change.
    window.claim_input().map_err(|_| LocalStop::NativeFailure)?;
    let binding = viewer.input_result_binding();
    let stop = Arc::new(AtomicBool::new(false));
    let released = Arc::new(AtomicBool::new(false));
    let mut failed = LocalStop::NativeFailure;
    viewer
        .capture_input_owned(&layout.clone(), |source| {
            let sampled = source.clock().map_err(|_| ())?;
            let capture_clock = CaptureClock::new(event_time().ok_or(())?, sampled.0);
            // Spend this preference once, even if native installation fails.
            // A replacement viewer never silently reactivates global capture.
            let shortcuts = if system_shortcuts.replace(false) {
                Some(window.bind_system_shortcuts(stop.clone()).map_err(|_| {
                    failed = LocalStop::SystemShortcutsUnavailable;
                })?)
            } else {
                None
            };
            let capture = Capture {
                stop: stop.clone(),
                released: released.clone(),
                shortcuts: shortcuts.clone(),
            };
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
                capture_until: 0,
                shortcuts,
                shortcut_context: None,
                shortcut_serial: 0,
                shortcut_local_failure: Cell::new(false),
                paused: false,
                last: None,
            });
            Ok::<_, ()>(capture)
        })
        .map_err(|_| failed)?;
    Ok(true)
}

impl Application for Session {
    type Output = ();
    // The same window part the renderer below opens 1:1. A resize reopen
    // replaced `fit_window` with the new window's size before this attempt.
    fn picture(&mut self, _attempt: u8, original: Option<(u32, u32)>) -> Option<(u32, u32)> {
        if self.picture_box.is_some() {
            self.stated_picture()
        } else {
            original
        }
    }
    fn offer(&mut self, attempt: u8, original: &Offer) -> Result<Offer, CallbackError> {
        if let Some(automation) = &self.automation
            && (attempt != 1 || self.report.borrow().attempts != 0 || !self.automation_profile())
        {
            automation.retire();
            return Err(CallbackError);
        }
        if let Some(once) = &self.once {
            if !once.accepts_attempt(attempt) || !self.one_shot_profile() {
                return Err(CallbackError);
            }
            return macos_observe_once::offer(original).map_err(|()| CallbackError);
        }
        let available = self
            .files
            .borrow()
            .offer(self.clipboard.borrow().available());
        let microphone = self
            .microphone
            .borrow()
            .as_ref()
            .is_some_and(|owner| owner.may_offer());
        let keep = |name: &str| {
            if name == fr_wire::audio_up::CAPABILITY {
                return microphone;
            }
            if name == fr_wire::sync_job_state::CAPABILITY {
                return self.files.borrow().folder_sync_available();
            }
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
        // Nothing is added here: the reconnect loop refuses a selection with
        // any capability its original lacks. Optional control capabilities
        // belong in the original (`control_offer`).
        let mut offer = original.clone();
        offer.capabilities.retain(|cap| keep(&cap.name));
        Ok(offer)
    }
    #[allow(clippy::too_many_lines)] // one attempt: open, serve, control
    async fn run(&mut self, attempt: u8, viewer: Viewer) -> Result<(), ObserverError> {
        if self.started
            || self.native_started.get()
            || self.observer.is_some()
            || self.audio_owner.borrow().is_some()
        {
            return Err(ObserverError::Order);
        }
        let one_shot = self.once.is_some();
        if one_shot && !self.one_shot_profile() {
            return Err(ObserverError::Order);
        }
        if let Some(automation) = &self.automation
            && (attempt != 1 || self.report.borrow().attempts != 0 || !self.automation_profile())
        {
            automation.retire();
            return Err(ObserverError::Order);
        }
        if let Some(once) = &mut self.once {
            once.begin(attempt).map_err(|()| ObserverError::Order)?;
        }
        let observation = self
            .observations
            .as_ref()
            .map(|publisher| publisher.original(attempt))
            .transpose()
            .map_err(|()| ObserverError::Application)?;
        self.started = true;
        self.service_succeeded = false;
        self.service_failure = None;
        self.cleanup_confirmed = false;
        self.report.borrow_mut().attempts = attempt;
        let stop = viewer.control();
        let window = self.window.clone();
        let input = self.input.clone();
        let files = self.files.clone();
        let clipboard = self.clipboard.clone();
        let clipboard_requested = clipboard.borrow().available();
        let audio_requested = self.audio_install.is_some();
        let system_shortcuts = self.system_shortcuts.clone();
        let shortcuts_requested = system_shortcuts.get();
        let stop_control_requested = self.control.is_some() && self.close_cx.is_some();
        let audio_owner = self.audio_owner.clone();
        let microphone = self.microphone.clone();
        let automation = self.automation.clone();
        let automated = automation.is_some();
        let file_directions = sharing::flags(&files.borrow()).count_ones();
        let file_selections = files.borrow().file_drop_requested();
        let text_requested = !automated
            && self
                .control
                .is_some_and(|policy| policy.capabilities.contains(Capability::Text));
        let sharing_flags = sharing::flags(&files.borrow())
            | if one_shot || automated { 0 } else { 4 }
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
            }
            | if audio_requested { AUDIO_FLAG } else { 0 }
            | if stop_control_requested {
                STOP_CONTROL_FLAG
            } else {
                0
            }
            | if shortcuts_requested {
                SYSTEM_SHORTCUTS_FLAG
            } else {
                0
            };
        let report = self.report.clone();
        let closed = self.closed.clone();
        let intent = self.intent.clone();
        let reopen_intent = self.reopen_intent.clone();
        let resize = self.resize.clone();
        let can_resize = Cell::new(false);
        let close_cx = self.close_cx.clone();
        let switchable = !one_shot && (self.control.is_none() || close_cx.is_some());
        reopen_intent.borrow_mut().set_mode(
            if switchable {
                ReopenMode::Viewing
            } else {
                ReopenMode::Disabled
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
        let picture_box = self.picture_box;
        let placement = self.placement;
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
        let approval_observation = observation.clone();
        let native_started = self.native_started.clone();
        let opening = viewer.observe_with_renderer(
            move |display, _original| async move {
                let native = (display.pixel_width, display.pixel_height);
                // A scaling host sends the largest picture inside the stated
                // box; open its window at one point per `scale` pixels so it
                // is drawn 1:1. Otherwise the remote's logical size applies.
                let (pixels, logical) = picture_box
                    .and_then(|screen| {
                        let full_screen = placement.is_some_and(|placement| placement.full_screen);
                        let within = screen.within(fit_window, full_screen)?;
                        let expected = display.picture(Some(within)).ok()?;
                        let points = crate::macos_sizing::scaled_picture_points(
                            native,
                            expected,
                            screen.scale,
                        )?;
                        Some((expected, points))
                    })
                    .unwrap_or((native, (display.logical_width, display.logical_height)));
                let points = crate::macos_sizing::sharing_points_with_shortcuts(
                    pixels,
                    logical,
                    fit_window,
                    file_directions,
                    clipboard_requested,
                    text_requested,
                    file_selections,
                    audio_requested,
                    stop_control_requested,
                    shortcuts_requested,
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
                if let Some(placement) = placement {
                    opened.place(placement).map_err(|_| ())?;
                    if placement.full_screen {
                        opened.enter_full_screen().map_err(|_| ())?;
                    }
                }
                clipboard_ui.borrow_mut().install(&opened);
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
                if let Some(observation) = &approval_observation {
                    observation.approval()?;
                }
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
                &reopen_intent,
                &resize,
                can_resize.get(),
                automation.as_deref(),
                &closed,
                &stop,
                &files,
                &clipboard,
                &mut sharing,
                &mut events,
            );
            if let Some(automation) = &automation
                && (stop.is_stopped() || closed.get() || reopen_intent.borrow().pending())
            {
                automation.retire();
            }
            service_audio(
                &window,
                &audio_owner,
                stop.is_stopped() || closed.get() || reopen_intent.borrow().pending(),
            );
            service_microphone(
                &window,
                &microphone,
                &report,
                stop.is_stopped() || closed.get() || reopen_intent.borrow().pending(),
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
        // frd decodes and submits the opening picture before serving; its
        // callbacks carry only later pictures (see `take_shown`).
        let opening_picture = submitted(opened.initial_presentation());
        let observer = self.observer.insert(opened);
        if let Some(once) = &mut self.once {
            let initial = observer.initial_presentation();
            let received_at = observer.initial_presentation_received_at();
            let snapshot = if once.screenshot() {
                match pumped(Box::pin(observer.snapshot_initial()), &mut pump).await {
                    Ok(snapshot) => snapshot,
                    Err(error) => {
                        once.discard();
                        self.service_failure = Some(error);
                        return Err(error);
                    }
                }
            } else {
                None
            };
            once.retain(InitialObservation {
                display,
                serial: initial.frame.as_raw(),
                evidence: observation_evidence(initial.stage),
                received_at,
                snapshot,
            })
            .map_err(|()| ObserverError::Application)?;
        }
        if let Some(observation) = &observation {
            observation
                .bind_snapshot(observer.snapshot_handle()?)
                .map_err(|()| ObserverError::Application)?;
            let initial = observer.initial_presentation();
            // The first receipt predates bootstrap acknowledgement waits. Keep
            // its actual local timestamp; registration cannot renew its age.
            observation
                .ready(
                    display,
                    initial.frame.as_raw(),
                    observation_evidence(initial.stage),
                    observer.initial_presentation_received_at(),
                )
                .map_err(|()| ObserverError::Application)?;
        }
        if let Some(automation) = &self.automation {
            automation
                .ready(
                    observer.snapshot_handle()?,
                    display,
                    observer.initial_presentation(),
                    observer.initial_presentation_received_at(),
                )
                .map_err(|_| ObserverError::Application)?;
        }
        if let Some(install) = &mut self.audio_install {
            *self.audio_owner.borrow_mut() = Some(Rc::from(install(observer)));
        }
        // Startup owns a real first frame; a missing repaint is never inferred
        // to be a resize. The next current native sample can arm its watcher.
        can_resize.set(!one_shot);
        files.borrow_mut().configure(observer);
        clipboard.borrow_mut().configure(
            observer,
            window.borrow().as_ref(),
            self.close_cx.clone().or_else(Cx::current),
        );
        let count = report.clone();
        // Per attempt: the gap before a reopened window's first picture is
        // not a frame interval.
        let mut previous: Option<std::time::Instant> = None;
        let frame_automation = self.automation.clone();
        let mut frames = move |frame: Option<Presentation>| {
            if let Some(frame) = frame {
                let now = Instant::now();
                if let Some(automation) = &frame_automation {
                    automation.presented(frame).map_err(|_| ())?;
                }
                if let Some(observation) = &observation {
                    observation.presented(
                        frame.frame.as_raw(),
                        observation_evidence(frame.stage),
                        now,
                    )?;
                }
                let mut report = count.borrow_mut();
                report.presented += 1;
                if let Some(previous) = previous.replace(now) {
                    let gap = now.saturating_duration_since(previous).as_micros();
                    report
                        .frame_intervals
                        .record(u64::try_from(gap).unwrap_or(u64::MAX));
                }
            }
            Ok::<(), ()>(())
        };
        let outcome = match control {
            None => {
                let closing = closed.clone();
                let switching = reopen_intent.clone();
                let ui_window = window.clone();
                let served = observer.serve_until(move |frame| {
                    macos_observe_once::flow(one_shot, || {
                        frames(frame)?;
                        let eligible = ui_window
                            .borrow()
                            .as_ref()
                            .and_then(|window| window.state().ok())
                            .is_some_and(|state| state.key && state.visible);
                        let mode = if eligible {
                            ReopenMode::Viewing
                        } else {
                            ReopenMode::Disabled
                        };
                        switching.borrow_mut().set_mode(mode, event_time());
                        if switching.borrow_mut().reject_ineligible(mode) {
                            return Err(());
                        }
                        Ok(if closing.get() || switching.borrow_mut().begin_close() {
                            std::ops::ControlFlow::Break(fr_wire::closure::Reason::Requested)
                        } else {
                            std::ops::ControlFlow::Continue(())
                        })
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
                let mut pending = opening_picture;
                let mut attached = false;
                let ui_window = window.clone();
                let ui_input = input.clone();
                let ui_report = report.clone();
                let ui_intent = intent.clone();
                let switching = reopen_intent.clone();
                let results = report.clone();
                let ui_resize = resize.clone();
                let ui_microphone = self.microphone.clone();
                let ui_automation = self.automation.clone();
                let result_automation = self.automation.clone();
                let ui_stop = stop.clone();
                let mut ui = move |state: InteractiveViewerState<'_>, frame| {
                    frames(frame)?;
                    let eligible = switchable
                        && ui_window
                            .borrow()
                            .as_ref()
                            .and_then(|window| window.state().ok())
                            .is_some_and(|state| state.key && state.visible);
                    let mode = match &state {
                        InteractiveViewerState::Viewing(_) if eligible => ReopenMode::Viewing,
                        InteractiveViewerState::Controlled(_) if eligible => ReopenMode::Controlled,
                        _ => ReopenMode::Disabled,
                    };
                    switching.borrow_mut().set_mode(mode, event_time());
                    if switching.borrow_mut().reject_ineligible(mode) {
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
                    let fresh =
                        shown(&ui_window, &mut pending, frame).filter(|f| last_shown != Some(*f));
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
                            if fresh.is_some() {
                                witness(&view.visible(frame), frame, &mut pending, &mut last_shown);
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
                                    .set_mode(ReopenMode::Disabled, event_time());
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
                                attached = attach(
                                    viewer,
                                    display,
                                    &ui_window,
                                    &ui_input,
                                    &ui_resize,
                                    ui_automation.is_none(),
                                    &system_shortcuts,
                                )
                                .map_err(|reason| {
                                    ui_report.borrow_mut().local_stop.get_or_insert(reason);
                                })?;
                            }
                            if let Some(automation) = &ui_automation {
                                let current = ui_window
                                    .borrow()
                                    .as_ref()
                                    .and_then(|window| window.state().ok())
                                    .is_some_and(super::WindowState::input_ready);
                                if !current || ui_stop.is_stopped() || ui_resize.changed() {
                                    automation.retire();
                                    viewer.close();
                                    return Err(());
                                }
                                if attached {
                                    automation.turn(viewer);
                                }
                            }
                            if let Some(input) = ui_input.borrow_mut().as_mut() {
                                input.refresh_capture(viewer).map_err(|()| {
                                    if input.shortcuts.is_some() {
                                        ui_report
                                            .borrow_mut()
                                            .local_stop
                                            .get_or_insert(LocalStop::SystemShortcutsStopped);
                                    }
                                })?;
                            }
                            if let Some(microphone) = ui_microphone.borrow_mut().as_mut() {
                                microphone.turn(viewer).map_err(|_| ())?;
                                ui_report.borrow_mut().microphone = Some(microphone.report());
                            }
                        }
                    }
                    Ok(std::ops::ControlFlow::Continue(()))
                };
                let result = move |result| {
                    if let Some(automation) = &result_automation {
                        automation.result(result);
                    }
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
        self.service_failure = outcome.as_ref().err().copied();
        if outcome.is_err()
            && let Some(once) = &mut self.once
        {
            once.discard();
        }
        outcome
    }
    fn cleanup(
        &mut self,
        cx: &Cx,
        deadline: Deadline,
    ) -> impl Future<Output = Result<(), CallbackError>> {
        // Fence at call time, before any AppKit/IME cleanup can block.
        let observations_retired = self
            .observations
            .as_ref()
            .is_none_or(|publisher| publisher.retire(Phase::Cleaning).is_ok());
        if let Some(automation) = &self.automation {
            automation.retire();
        }
        self.input.borrow_mut().take();
        if let Some(observer) = &self.observer {
            observer.control().stop();
        }
        if let Some(audio) = self.audio_owner.borrow().as_ref() {
            audio.stop();
        }
        if let Some(microphone) = self.microphone.borrow_mut().as_mut() {
            microphone.stop();
            self.report.borrow_mut().microphone = Some(microphone.report());
        }
        let shortcuts_retired = retire_shortcuts(&self.window).is_ok();
        self.seal_window_resize();
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
                        report.media = Some(observer.statistics());
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
            let audio_owner = self.audio_owner.borrow().clone();
            let audio = reap_audio(audio_owner.as_deref(), cx, deadline).await;
            let microphone = reap_microphone(&self.microphone, cx, deadline).await;
            if !result
                || !observations_retired
                || !text_retired
                || !shortcuts_retired
                || !picker_closed
                || !commands
                || !clipboard
                || !audio
                || !microphone
            {
                return Err(CallbackError);
            }
            self.observer = None;
            self.audio_owner.borrow_mut().take();
            window.borrow_mut().take();
            self.picker.borrow_mut().take();
            self.started = false;
            self.native_started.set(false);
            self.cleanup_confirmed = true;
            Ok(())
        }
    }
    fn status(&mut self, status: Status) -> Result<(), CallbackError> {
        if let Some(automation) = &self.automation
            && !matches!(
                status,
                Status::Connecting { .. } | Status::Authenticated { .. }
            )
        {
            automation.retire();
        }
        match status {
            Status::Connecting { attempt } => {
                if let Some(automation) = &self.automation
                    && (attempt != 1
                        || self.report.borrow().attempts != 0
                        || !self.automation_profile())
                {
                    automation.retire();
                    return Err(CallbackError);
                }
                if self
                    .once
                    .as_ref()
                    .is_some_and(|once| !once.accepts_attempt(attempt))
                {
                    return Err(CallbackError);
                }
                if self.started
                    || self.native_started.get()
                    || self.observer.is_some()
                    || self.audio_owner.borrow().is_some()
                    || self.picker.borrow().is_some()
                    || !self.files.borrow().commands_complete()
                    || !self.clipboard.borrow_mut().finished()
                {
                    return Err(CallbackError);
                }
                self.native_started.set(false);
                // The old window and callback Box were positively retired.
                // A late reference to its policy cannot affect this new owner.
                self.resize = Rc::default();
                self.cleanup_confirmed = false;
                self.terminal = None;
                self.picker_error.set(None);
                self.service_succeeded = false;
                self.service_failure = None;
                self.report.borrow_mut().close_exchange = None;
                self.report.borrow_mut().lease_revocation = None;
                self.intent.borrow_mut().begin_attempt();
                self.reopen_intent.borrow_mut().begin_attempt();
                Ok(())
            }
            Status::Reconnecting { .. } if self.reopen_intent.borrow().pending() => {
                Err(CallbackError)
            }
            Status::Reconnecting { .. } | Status::Reopening { .. }
                if self.closed.get() || self.report.borrow().local_stop.is_some() =>
            {
                Err(CallbackError)
            }
            _ => Ok(()),
        }?;
        if let Some(publisher) = &self.observations {
            match status {
                Status::Connecting { attempt } => publisher.begin(attempt),
                Status::Authenticated { attempt } => publisher.authenticated(attempt),
                Status::Cleaning { .. } => publisher.retire(Phase::Cleaning),
                Status::Reconnecting { .. } | Status::Reopening { .. } => {
                    publisher.retire(Phase::Reconnecting)
                }
                Status::Completed { .. } | Status::Stopped { .. } => {
                    publisher.retire(Phase::Stopped)
                }
            }
            .map_err(|()| CallbackError)?;
        }
        Ok(())
    }
    fn completion_action(&mut self) -> Result<CompletionAction, CallbackError> {
        // The explicit robot publisher owns one attempt. A successful local
        // display choice must not spend the supervisor's separate reopen path
        // or clear the original receipt/cleanup evidence in a new Connecting.
        if self.once.is_some()
            || self.observations.is_some()
            || self.automation.is_some()
            || !self.reopen_intent.borrow().pending()
        {
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
            || report
                .display_change_closes
                .len()
                .saturating_add(report.control_release_closes.len())
                >= 31
        {
            return Err(CallbackError);
        }
        let closed = DisplayChangeReport {
            attempt: report.attempts,
            close_exchange: report.close_exchange,
            lease_revocation: report.lease_revocation,
        };
        drop(report);
        let action = self
            .reopen_intent
            .borrow_mut()
            .consume()
            .ok_or(CallbackError)?;
        let mut report = self.report.borrow_mut();
        match action {
            ReopenAction::ChangeDisplay => {
                report.display_changes += 1;
                report.display_change_closes.push(closed);
                self.display_picker = true;
            }
            ReopenAction::StopControl => {
                report.control_releases += 1;
                report.control_release_closes.push(closed);
            }
        }
        // Every attempt re-evaluates the configured chooser against the fresh
        // approved catalog. No old display alias is retained by either action.
        self.intent.borrow_mut().retire_initial();
        // Original handles remain for the completion report; no new observer
        // can acquire another source or destination from this invocation.
        self.files.borrow_mut().cancel();
        Ok(CompletionAction::Reopen)
    }
    fn local_layout_change(
        &mut self,
        failure: reconnect::Failure,
    ) -> Result<Option<LocalLayoutChange>, CallbackError> {
        if self.once.is_some() || self.observations.is_some() || self.automation.is_some() {
            return Ok(None);
        }
        Ok(self.window_resize_action(failure, super::event_time()))
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

    #[test]
    fn shortcut_consent_is_exclusive_to_a_native_control_owner() {
        let selected = Session::new(
            Some(ControlPolicy::default()),
            ObserverPolicy::default(),
            Box::new(|_| Ok(None)),
            1,
            "shortcuts".into(),
        )
        .with_system_shortcuts(true);
        assert!(selected.system_shortcuts.get());
        assert!(!selected.automation_profile());
        // The capture installer consumes the preference before its native
        // call, including a refused attempt. No fresh UI request can reset it.
        assert!(selected.system_shortcuts.replace(false));
        assert!(!selected.system_shortcuts.replace(false));
        let viewing = Session::new(
            None,
            ObserverPolicy::default(),
            Box::new(|_| Ok(None)),
            1,
            "view".into(),
        )
        .with_system_shortcuts(true);
        assert!(!viewing.system_shortcuts.get());
        assert!(viewing.one_shot_profile());
    }

    #[test]
    fn the_opening_picture_counts_once_visible_even_late_and_a_clock_refusal_reports_it_again() {
        use fr_media::access_unit::FrameId;
        let picture = |raw, stage| Presentation {
            frame: FrameId::from_raw(raw),
            stage,
        };
        let submitted_at = |raw| Some(picture(raw, PresentationStage::SubmittedToCompositor));
        // Planted negative: frd's callbacks never carry the opening picture,
        // so the old rule (only a callback picture counts) never reported it.
        assert_eq!(take_shown(&mut None, None, true), None);
        let mut pending = submitted(picture(3, PresentationStage::SubmittedToCompositor));
        // The window is not visible yet: it stays pending.
        assert_eq!(take_shown(&mut pending, None, false), None);
        assert_eq!(take_shown(&mut pending, None, true), Some(3));
        assert_eq!(take_shown(&mut pending, None, true), None, "reported once");
        // Before the clock is correlated the view does not exist: the refusal
        // puts the picture back for a later turn.
        let mut last_shown = None;
        let not_ready: Result<(), _> = Err(StreamingViewerError::Control(
            ControlledViewerError::ClockNotReady,
        ));
        witness(&not_ready, 3, &mut pending, &mut last_shown);
        assert_eq!(last_shown, None);
        assert_eq!(take_shown(&mut pending, None, true), Some(3));
        // Any other refusal waits for a newer picture.
        witness(
            &Err::<(), _>(StreamingViewerError::Closed),
            3,
            &mut pending,
            &mut last_shown,
        );
        assert_eq!(take_shown(&mut pending, None, true), None);
        witness(
            &Ok::<(), StreamingViewerError>(()),
            3,
            &mut pending,
            &mut last_shown,
        );
        assert_eq!(last_shown, Some(3));
        // Planted negative (the old rule set `last_shown` only on Ok): a late
        // picture is on screen, held unqualified by frd until the host
        // verifies it. It counts as shown, so the view keeps checking its
        // gated evidence, and it is not reported again.
        let (mut late_pending, mut late_shown) = (None, None);
        witness(
            &Err::<(), _>(StreamingViewerError::Freshness(
                FreshnessError::QueueExpired,
            )),
            4,
            &mut late_pending,
            &mut late_shown,
        );
        assert_eq!(late_shown, Some(4));
        assert_eq!(late_pending, None);
        // A newer callback picture replaces a pending one; decode-only never counts.
        let mut pending = submitted(picture(4, PresentationStage::SubmittedToCompositor));
        assert_eq!(take_shown(&mut pending, submitted_at(5), true), Some(5));
        let mut pending = submitted(picture(6, PresentationStage::SubmittedToCompositor));
        let decoded = Some(picture(7, PresentationStage::DecodedOnly));
        assert_eq!(take_shown(&mut pending, decoded, true), None);
        assert_eq!(submitted(picture(8, PresentationStage::DecodedOnly)), None);
    }

    #[derive(Default)]
    struct AudioCleanup {
        stopped: Cell<bool>,
        finished: Cell<bool>,
    }
    impl AudioRetirement for AudioCleanup {
        fn stop(&self) {
            self.stopped.set(true);
        }
        fn is_complete(&self) -> bool {
            self.finished.get()
        }
        fn view(&self) -> AudioView {
            AudioView {
                state: AudioState::Starting,
                volume_percent: 100,
                muted: false,
            }
        }
        fn preferences(&self, _: u8, _: bool) {
            panic!("no native controls in this ownership fixture");
        }
        fn controls_failed(&self) {
            self.stop();
        }
    }

    #[test]
    fn unresolved_audio_cleanup_fences_synchronously_and_prevents_reconnect() {
        let runtime = RuntimeBuilder::current_thread().build().unwrap();
        let cx = runtime.request_cx_with_budget(Budget::INFINITE);
        let deadline = Deadline::after(&cx, Duration::from_millis(10)).unwrap();
        let mut session = failed_startup(false);
        let original = Rc::new(AudioCleanup::default());
        *session.audio_owner.borrow_mut() = Some(original.clone());

        let cleanup = session.cleanup(&cx, deadline);
        assert!(
            original.stopped.get(),
            "fence before polling any native cleanup"
        );
        assert_eq!(runtime.block_on(cleanup), Err(CallbackError));
        assert!(session.audio_owner.borrow().is_some());
        assert!(!session.cleanup_confirmed());
        assert_eq!(
            session.status(Status::Connecting { attempt: 2 }),
            Err(CallbackError)
        );

        original.finished.set(true);
        let deadline = Deadline::after(&cx, Duration::from_secs(1)).unwrap();
        assert_eq!(runtime.block_on(session.cleanup(&cx, deadline)), Ok(()));
        assert!(session.audio_owner.borrow().is_none());
        session.status(Status::Connecting { attempt: 2 }).unwrap();
    }

    #[test]
    fn dropping_session_fences_original_audio_without_certifying_retirement() {
        let session = failed_startup(false);
        let original = Rc::new(AudioCleanup::default());
        *session.audio_owner.borrow_mut() = Some(original.clone());
        drop(session);
        assert!(original.stopped.get());
        assert!(!original.finished.get());
    }

    fn selected_close() -> Session {
        selected_action(ReopenAction::ChangeDisplay)
    }

    fn selected_action(action: ReopenAction) -> Session {
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
        let mut intent = session.reopen_intent.borrow_mut();
        intent.set_mode(
            if action == ReopenAction::StopControl {
                ReopenMode::Controlled
            } else {
                ReopenMode::Viewing
            },
            Some(Duration::from_millis(1)),
        );
        assert!(intent.request(
            action,
            Duration::from_millis(2),
            Some(Duration::from_millis(3))
        ));
        assert!(intent.begin_close());
        drop(intent);
        session
    }

    fn resized_session() -> Session {
        resized_session_to(1280.0, 840.0, 0)
    }

    fn resized_session_to(width: f64, height: f64, full_screen: i32) -> Session {
        // Policy fixture only; no window, native owner, host or grant exists.
        let mut session = failed_startup(false);
        session.started = false;
        session.cleanup_confirmed = true;
        session.terminal = None;
        session.service_failure = Some(ObserverError::Streaming(
            frd::session_startup::StreamingViewerError::Closed,
        ));
        session.intent = Rc::new(RefCell::new(Intent::new(true)));
        session.report.borrow_mut().local_stop = Some(LocalStop::PictureChanged);
        // `run` records its attempt before any geometry can be observed.
        session.report.borrow_mut().attempts = 1;
        let initial = crate::macos_resize::Sample {
            revision: 1,
            width: 960.0,
            height: 660.0,
            x: 40.0,
            y: 80.0,
            observed: 0.1,
            eligible: 1,
            full_screen: 0,
        };
        assert!(session.resize.arm(initial));
        let changed = crate::macos_resize::Sample {
            revision: 2,
            width,
            height,
            observed: 0.11,
            full_screen,
            ..initial
        };
        session.resize.observe(changed, false);
        assert!(session.resize.seal(crate::macos_resize::Sample {
            observed: 0.12,
            ..changed
        }));
        session
    }

    #[test]
    fn observation_publication_keeps_one_attempt_and_its_original_cleanup_evidence() {
        // Use the actual Session hooks with the existing policy fixtures. No
        // native window, remote grant or cleanup execution is claimed here.
        let mut display = selected_close();
        display.observations = Some(macos_live_observation::Publisher::new(|_| Ok(())));
        let original = display.report();
        assert_eq!(display.completion_action(), Ok(CompletionAction::Finish));
        assert!(display.cleanup_confirmed());
        assert_eq!(display.report().attempts, original.attempts);
        assert_eq!(display.report().close_exchange, original.close_exchange);
        assert_eq!(display.report().lease_revocation, original.lease_revocation);
        assert_eq!(display.report().display_changes, 0);
        assert!(!display.display_picker);

        let mut resize = resized_session();
        resize.observations = Some(macos_live_observation::Publisher::new(|_| Ok(())));
        resize.report.borrow_mut().close_exchange = original.close_exchange;
        let original = resize.report();
        assert_eq!(
            resize.local_layout_change(reconnect::Failure::Cancelled),
            Ok(None)
        );
        assert!(resize.cleanup_confirmed());
        assert_eq!(resize.report().attempts, original.attempts);
        assert_eq!(resize.report().close_exchange, original.close_exchange);
        assert_eq!(resize.report().local_stop, original.local_stop);
        assert_eq!(resize.report().window_resizes, 0);
        assert!(resize.fit_window.is_none());
    }

    #[derive(Clone, Default)]
    struct AutomationFence(Rc<Cell<bool>>);
    impl Automation for AutomationFence {
        fn ready(
            &self,
            _: fr_client::robot::snapshot::Handle,
            _: Display,
            _: Presentation,
            _: Instant,
        ) -> Result<(), CallbackError> {
            panic!("lifecycle fixture creates no renderer");
        }
        fn presented(&self, _: Presentation) -> Result<(), CallbackError> {
            panic!("lifecycle fixture creates no renderer");
        }
        fn turn(&self, _: &mut ControlledViewer) {
            panic!("lifecycle fixture creates no granted controller");
        }
        fn result(&self, _: fr_client::input::ResultEvent) {
            panic!("lifecycle fixture submits no input");
        }
        fn retire(&self) {
            self.0.set(true);
        }
    }

    fn automation_session() -> Session {
        Session::new(
            Some(ControlPolicy::default()),
            ObserverPolicy::default(),
            Box::new(|_| Ok(None)),
            1,
            "automation lifecycle fixture".to_owned(),
        )
    }

    #[test]
    fn automation_requires_exclusive_control_before_the_original_attempt() {
        assert!(
            automation_session()
                .with_automation(AutomationFence::default())
                .is_ok()
        );
        let mut viewing = automation_session();
        viewing.control = None;
        assert!(viewing.with_automation(AutomationFence::default()).is_err());
        let mut observing = automation_session();
        observing.observations = Some(macos_live_observation::Publisher::new(|_| Ok(())));
        assert!(
            observing
                .with_automation(AutomationFence::default())
                .is_err()
        );
        let mut once = automation_session();
        once.once = Some(Once::new(false));
        assert!(once.with_automation(AutomationFence::default()).is_err());
        let mut choosing = automation_session();
        choosing.display_picker = true;
        assert!(
            choosing
                .with_automation(AutomationFence::default())
                .is_err()
        );
        let mut started = automation_session();
        started.started = true;
        assert!(started.with_automation(AutomationFence::default()).is_err());
        let previous = automation_session();
        previous.report.borrow_mut().attempts = 1;
        assert!(
            previous
                .with_automation(AutomationFence::default())
                .is_err()
        );

        let automated = automation_session()
            .with_automation(AutomationFence::default())
            .unwrap();
        assert!(automated.with_observations(|_| Ok(())).is_err());
    }

    #[test]
    fn automation_never_reopens_or_resets_original_attempt_evidence() {
        for action in [ReopenAction::ChangeDisplay, ReopenAction::StopControl] {
            let mut session = selected_action(action);
            session.control = Some(ControlPolicy::default());
            let fence = AutomationFence::default();
            session.automation = Some(Rc::new(fence.clone()));
            let original = session.report();
            assert_eq!(session.completion_action(), Ok(CompletionAction::Finish));
            for attempt in [2, 1] {
                assert_eq!(
                    session.status(Status::Connecting { attempt }),
                    Err(CallbackError)
                );
                assert!(fence.0.get());
                assert!(session.cleanup_confirmed());
                assert_eq!(session.report().attempts, original.attempts);
                assert_eq!(session.report().close_exchange, original.close_exchange);
                assert_eq!(session.report().lease_revocation, original.lease_revocation);
            }
            assert_eq!(session.report().display_changes, 0);
        }
        let mut resized = resized_session();
        resized.automation = Some(Rc::new(AutomationFence::default()));
        let original = resized.report();
        assert_eq!(
            resized.local_layout_change(reconnect::Failure::Cancelled),
            Ok(None)
        );
        assert_eq!(resized.report().local_stop, original.local_stop);
        assert_eq!(resized.report().window_resizes, 0);
        assert!(resized.fit_window.is_none());
    }

    #[test]
    fn automation_retires_before_cleanup_is_polled_and_before_status_refusal() {
        let runtime = RuntimeBuilder::current_thread().build().unwrap();
        let cx = runtime.request_cx_with_budget(Budget::INFINITE);
        let deadline = Deadline::after(&cx, Duration::from_secs(1)).unwrap();
        let fence = AutomationFence::default();
        let mut session = automation_session().with_automation(fence.clone()).unwrap();
        let cleanup = session.cleanup(&cx, deadline);
        assert!(
            fence.0.get(),
            "fence must not wait for cleanup to be polled"
        );
        drop(cleanup);
        assert!(!session.cleanup_confirmed());

        let mut closing = selected_close();
        let fence = AutomationFence::default();
        closing.automation = Some(Rc::new(fence.clone()));
        assert_eq!(
            closing.status(Status::Reconnecting {
                attempt: 2,
                delay: Duration::ZERO,
                failure: reconnect::Failure::Cancelled,
            }),
            Err(CallbackError)
        );
        assert!(fence.0.get());

        let fence = AutomationFence::default();
        drop(automation_session().with_automation(fence.clone()).unwrap());
        assert!(fence.0.get());
    }

    #[test]
    fn a_resized_window_states_its_own_box_on_the_reopened_attempt() {
        // mac-mini-old's screen box: 1920x1080 at 1x with the Dock shown.
        let screen = crate::macos::WindowBox {
            pixels: (1920, 933),
            scale: 1,
            reserved: 44,
        };
        let mut session = resized_session().with_picture_box(Some(screen));
        assert_eq!(
            Application::picture(&mut session, 1, Some((1920, 933))),
            Some((1920, 933))
        );
        assert_eq!(
            session.window_resize_action(
                reconnect::Failure::Cancelled,
                Some(Duration::from_millis(130))
            ),
            Some(LocalLayoutChange::WindowResize),
        );
        // The 1280x840-point window shows 1280x796 below the sharing strip.
        assert_eq!(
            Application::picture(&mut session, 2, Some((1920, 933))),
            Some((1280, 796))
        );
        // Without a screen box, the configured box is kept.
        assert_eq!(
            Application::picture(&mut resized_session(), 2, Some((1920, 1080))),
            Some((1920, 1080))
        );
    }

    #[test]
    fn a_full_screen_window_states_the_whole_screen_on_the_reopened_attempt() {
        // mac-mini-old: the windowed ceiling is 1920x933; full screen is
        // 1920x1080 points with no title bar, menu bar or Dock.
        let screen = crate::macos::WindowBox {
            pixels: (1920, 933),
            scale: 1,
            reserved: 44,
        };
        let mut session = resized_session_to(1920.0, 1080.0, 1).with_picture_box(Some(screen));
        assert_eq!(
            session.window_resize_action(
                reconnect::Failure::Cancelled,
                Some(Duration::from_millis(130))
            ),
            Some(LocalLayoutChange::WindowResize),
        );
        assert!(session.placement.unwrap().full_screen);
        assert_eq!(
            Application::picture(&mut session, 2, Some((1920, 933))),
            Some((1920, 1036))
        );
        // Leaving full screen states the windowed box again.
        let mut windowed = resized_session_to(1582.0, 931.0, 0).with_picture_box(Some(screen));
        assert_eq!(
            windowed.window_resize_action(
                reconnect::Failure::Cancelled,
                Some(Duration::from_millis(130))
            ),
            Some(LocalLayoutChange::WindowResize),
        );
        assert_eq!(
            Application::picture(&mut windowed, 2, Some((1920, 933))),
            Some((1582, 887))
        );
    }

    #[test]
    fn checked_geometry_hook_reopens_once_without_regrant_or_original_receipt_loss() {
        let mut session = resized_session();
        let failure = reconnect::Failure::Cancelled;
        assert_eq!(
            session.window_resize_action(failure, Some(Duration::from_millis(130))),
            Some(LocalLayoutChange::WindowResize),
        );
        assert_eq!(session.fit_window, Some((1280, 840)));
        assert_eq!(session.placement.unwrap().origin, (40, 80));
        assert!(session.intent.borrow().needs_explicit_request());
        let report = session.report();
        assert_eq!(report.window_resizes, 1);
        assert_eq!(report.window_resize_closes.len(), 1);
        assert_eq!(report.window_resize_closes[0].original.attempt, 1);
        assert!(
            report.window_resize_closes[0]
                .original
                .close_exchange
                .is_none()
        );
        assert!(
            report.window_resize_closes[0]
                .original
                .lease_revocation
                .is_none()
        );
        assert_eq!(report.window_resize_closes[0].interruption, failure);
        assert!(report.local_stop.is_none());
        assert_eq!(
            session.window_resize_action(failure, Some(Duration::from_millis(140))),
            None
        );
        session.status(Status::Connecting { attempt: 2 }).unwrap();
        assert!(!session.resize.armed());
        assert!(
            !session
                .intent
                .borrow_mut()
                .take_request(true, Some(Duration::from_millis(150)))
        );
        assert_eq!(session.report().window_resize_closes.len(), 1);
    }

    #[test]
    fn geometry_hook_refuses_unrelated_failures_terminal_locals_and_unreaped_owners() {
        for refusal in 0..11 {
            let mut session = resized_session();
            let mut failure = reconnect::Failure::Cancelled;
            match refusal {
                0 => session.cleanup_confirmed = false,
                1 => session.service_succeeded = true,
                2 => session.started = true,
                3 => session.native_started.set(true),
                4 => session.closed.set(true),
                5 => session.report.borrow_mut().local_stop = Some(LocalStop::FocusLost),
                6 => session.terminal = Some(ObserverError::Application),
                7 => session.report.borrow_mut().attempts = 32,
                8 => session.service_failure = Some(ObserverError::Application),
                9 => failure = reconnect::Failure::Clock,
                10 => {
                    let mut display = session.reopen_intent.borrow_mut();
                    display.set_mode(ReopenMode::Viewing, Some(Duration::from_millis(1)));
                    assert!(display.request(
                        ReopenAction::ChangeDisplay,
                        Duration::from_millis(2),
                        Some(Duration::from_millis(3)),
                    ));
                    assert!(display.begin_close());
                }
                _ => unreachable!(),
            }
            assert_eq!(
                session.window_resize_action(failure, Some(Duration::from_millis(130))),
                None
            );
            assert_eq!(session.report().window_resizes, 0);
            assert!(session.fit_window.is_none());
        }
    }

    #[test]
    fn geometry_hook_keeps_reported_host_close_terminal_even_after_local_cancel() {
        use frd::session_startup::{Error, StreamingViewerError};
        for cleanup in [
            fr_wire::closure::Cleanup::Complete,
            fr_wire::closure::Cleanup::Incomplete,
        ] {
            let mut session = resized_session();
            let closed = fr_wire::closure::Closed {
                reason: fr_wire::closure::ClosedReason::ClientRequested,
                cleanup,
                effects: fr_wire::closure::OutstandingEffects::Unknown,
            };
            let outcome = fr_transport::quic::CloseOutcome {
                request_acknowledged: true,
                report: Some(closed),
                transport: Ok(()),
            };
            session.report.borrow_mut().close_exchange = Some(outcome);
            session.terminal = Some(ObserverError::Streaming(StreamingViewerError::Session(
                Error::RemoteClosed(closed),
            )));
            assert_eq!(
                session.window_resize_action(
                    reconnect::Failure::Cancelled,
                    Some(Duration::from_millis(130))
                ),
                None
            );
            assert_eq!(session.report().close_exchange, Some(outcome));
            assert!(session.report().window_resize_closes.is_empty());
        }
    }

    #[test]
    fn geometry_hook_cannot_spend_the_receipt_after_cleanup_used_its_original_lifetime() {
        let mut session = resized_session();
        assert_eq!(
            session.window_resize_action(
                reconnect::Failure::Cancelled,
                Some(Duration::from_millis(2110))
            ),
            None
        );
        assert_eq!(session.report().local_stop, Some(LocalStop::PictureChanged));
        assert!(session.report().window_resize_closes.is_empty());
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
        assert!(!session.reopen_intent.borrow().pending());
        assert!(session.report().close_exchange.is_none());
        assert_eq!(
            session.report().display_change_closes[0].close_exchange,
            original
        );
    }

    #[test]
    fn uncertain_cleanup_failed_service_adverse_terminal_and_local_stop_prevent_reopen() {
        for action in [ReopenAction::ChangeDisplay, ReopenAction::StopControl] {
            for refusal in 0..9 {
                let mut session = selected_action(action);
                match refusal {
                    0 => session.cleanup_confirmed = false,
                    1 => session.service_succeeded = false,
                    2 => session.terminal = Some(ObserverError::Application),
                    3 => session.closed.set(true),
                    4 => session.report.borrow_mut().local_stop = Some(LocalStop::FocusLost),
                    5 => session.report.borrow_mut().close_exchange = None,
                    6 => session.report.borrow_mut().attempts = 32,
                    7 => session.started = true,
                    8 => session.native_started.set(true),
                    _ => unreachable!(),
                }
                assert_eq!(session.completion_action(), Err(CallbackError));
                let report = session.report();
                assert_eq!(report.display_changes, 0);
                assert!(report.display_change_closes.is_empty());
                assert_eq!(report.control_releases, 0);
                assert!(report.control_release_closes.is_empty());
            }
        }
    }

    #[test]
    fn stop_control_reopens_viewing_once_with_a_fresh_choice_and_no_control_consent() {
        for picker in [false, true] {
            let mut session = selected_action(ReopenAction::StopControl);
            session.display_picker = picker;
            let original = session.report.borrow().close_exchange;
            assert_eq!(session.completion_action(), Ok(CompletionAction::Reopen));
            // Keep the invocation's fresh-catalog chooser; retain no display alias.
            assert_eq!(session.display_picker, picker);
            assert!(session.intent.borrow().needs_explicit_request());
            let report = session.report();
            assert_eq!(report.control_releases, 1);
            assert_eq!(report.control_release_closes.len(), 1);
            assert_eq!(report.control_release_closes[0].attempt, 1);
            assert_eq!(report.control_release_closes[0].close_exchange, original);
            assert_eq!(report.display_changes, 0);
            assert!(report.display_change_closes.is_empty());
            assert_eq!(session.completion_action(), Ok(CompletionAction::Finish));
            session.status(Status::Connecting { attempt: 2 }).unwrap();
            assert!(!session.reopen_intent.borrow().pending());
            assert!(session.report().close_exchange.is_none());
            assert_eq!(
                session.report().control_release_closes[0].close_exchange,
                original
            );
            assert!(
                !session
                    .intent
                    .borrow_mut()
                    .take_request(true, Some(Duration::from_millis(10)))
            );
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
        for action in [ReopenAction::ChangeDisplay, ReopenAction::StopControl] {
            let mut session = selected_action(action);
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
    }

    /// A control session's attempts only ever remove from the original offer:
    fn unstarted_observer() -> Session {
        Session::new(
            None,
            ObserverPolicy::default(),
            Box::new(|_| Ok(None)),
            1,
            "finite observation policy fixture".to_owned(),
        )
    }

    #[test]
    fn finite_observation_refuses_incompatible_owners_and_post_start_installation() {
        for case in 0..8 {
            let mut session = unstarted_observer();
            match case {
                0 => session.control = Some(ControlPolicy::default()),
                1 => session = session.with_observations(|_| Ok(())).unwrap(),
                2 => session.started = true,
                3 => session.native_started.set(true),
                4 => session.report.borrow_mut().attempts = 1,
                5 => session = session.with_display_picker(),
                6 => session = session.with_files(files::Files::default().with_commands()),
                7 => session.clipboard.borrow_mut().request(true),
                _ => unreachable!(),
            }
            assert!(session.observing_once(true).is_err(), "case {case}");
        }
        let session = unstarted_observer().observing_once(true).unwrap();
        assert!(session.with_observations(|_| Ok(())).is_err());
        let session = unstarted_observer().observing_once(false).unwrap();
        assert!(session.observing_once(true).is_err());
    }

    #[test]
    fn finite_observation_rechecks_profile_and_refuses_retry_and_display_reopen() {
        let original = fr_client::native::observation_offer();
        let mut session = unstarted_observer().observing_once(true).unwrap();
        assert_eq!(session.offer(1, &original).unwrap(), original);
        assert!(session.offer(2, &original).is_err());
        assert!(session.status(Status::Connecting { attempt: 2 }).is_err());
        assert_eq!(session.report().attempts, 0);
        session.status(Status::Connecting { attempt: 1 }).unwrap();
        assert!(session.take_initial_observation().is_none());
        let now = Duration::from_millis(10);
        session
            .reopen_intent
            .borrow_mut()
            .set_mode(ReopenMode::Viewing, Some(now));
        assert!(session.reopen_intent.borrow_mut().request(
            ReopenAction::ChangeDisplay,
            now,
            Some(now)
        ));
        assert!(session.reopen_intent.borrow_mut().begin_close());
        assert_eq!(
            session.completion_action().unwrap(),
            CompletionAction::Finish
        );
        assert_eq!(
            session
                .local_layout_change(reconnect::Failure::Cancelled)
                .unwrap(),
            None
        );
        assert!(!session.display_picker);
        session = session.with_display_picker();
        assert!(session.offer(1, &original).is_err());
    }

    #[test]
    fn retained_finite_receipt_needs_successful_cleanup_and_no_terminal_stop() {
        let at = Instant::now();
        for cause in 0..6 {
            let mut session = unstarted_observer().observing_once(false).unwrap();
            let once = session.once.as_mut().unwrap();
            once.begin(1).unwrap();
            once.retain(InitialObservation {
                display: Display {
                    handle: 17,
                    geometry: fr_core::ids::DisplayGeometryGeneration::INITIAL,
                    x: 0,
                    y: 0,
                    pixel_width: 320,
                    pixel_height: 240,
                    logical_width: 320,
                    logical_height: 240,
                    scale_numerator: 1,
                    scale_denominator: 1,
                    rotation: 0,
                },
                serial: 17,
                evidence: Evidence::SubmittedToCompositor,
                received_at: at,
                snapshot: None,
            })
            .unwrap();
            session.service_succeeded = true;
            session.cleanup_confirmed = true;
            match cause {
                0 => session.service_succeeded = false,
                1 => session.cleanup_confirmed = false,
                2 => session.closed.set(true),
                3 => session.report.borrow_mut().local_stop = Some(LocalStop::WindowClosed),
                4 => session.terminal = Some(ObserverError::Application),
                5 => {}
                _ => unreachable!(),
            }
            // A duplicate Connecting callback cannot reset the original
            // service/cleanup facts or negotiate a second authenticated view.
            assert!(session.status(Status::Connecting { attempt: 1 }).is_err());
            assert!(
                session
                    .offer(1, &fr_client::native::observation_offer())
                    .is_err()
            );
            let original = session.take_initial_observation();
            if cause == 5 {
                let original = original.unwrap();
                assert_eq!(original.serial, 17);
                assert_eq!(original.received_at, at);
                assert!(session.take_initial_observation().is_none());
            } else {
                assert!(original.is_none());
            }
        }
    }

    /// frd's reconnect loop refuses a selection holding any capability its
    /// original lacks. Adding precise scroll and committed text per attempt
    /// made every macOS control connection end `Notification` before dialing.
    #[test]
    fn a_control_session_selects_only_capabilities_its_original_offer_holds() {
        let mut session = Session::new(
            Some(ControlPolicy::default()),
            ObserverPolicy::default(),
            Box::new(|_| Ok(None)),
            1,
            "control offer fixture".to_owned(),
        );
        let original = session
            .control_offer(fr_client::native::control_offer())
            .unwrap();
        for name in [
            fr_wire::control::PIXEL_SCROLL_CAPABILITY,
            fr_wire::control::TEXT_CAPABILITY,
        ] {
            assert!(
                original
                    .capabilities
                    .iter()
                    .any(|cap| cap.name == name && !cap.required),
                "the original control offer requests optional {name}"
            );
        }
        for attempt in 1..=3 {
            assert_eq!(session.offer(attempt, &original).unwrap(), original);
        }
        // An original without them (an older caller) stays without them.
        let plain = fr_client::native::control_offer();
        assert_eq!(session.offer(1, &plain).unwrap(), plain);
        // Observation is never widened into a control request.
        let observing = fr_client::native::observation_offer();
        assert_eq!(session.control_offer(observing.clone()).unwrap(), observing);
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
    fn an_app_switch_pauses_a_grant_and_return_resumes_it_without_ending_control() {
        // The pause event is not terminal: only close, escape, a minimize or
        // hide (FocusLost) and overflow end the batch.
        assert_eq!(terminal_event(&[WindowEvent::FocusPaused]), None);
        assert_eq!(
            terminal_event(&[WindowEvent::FocusPaused, WindowEvent::FocusLost]),
            Some(LocalStop::FocusLost)
        );
        // Away: pause (again, harmlessly, while still away).
        assert_eq!(focus_step(false, true, true), FocusStep::Pause);
        assert_eq!(focus_step(true, true, false), FocusStep::Pause);
        // Back: resume once, then deliver normally.
        assert_eq!(focus_step(true, false, false), FocusStep::Resume);
        assert_eq!(focus_step(false, false, false), FocusStep::Continue);
        // Away and back within one poll: native forgot its held keys, so the
        // host's must still be released before input continues.
        assert_eq!(focus_step(false, false, true), FocusStep::Pause);
        assert_eq!(focus_step(true, false, true), FocusStep::Resume);
    }

    #[test]
    fn automation_fences_every_focus_pause_without_changing_native_input_policy() {
        let events = [WindowEvent::FocusPaused];
        assert_eq!(producer_terminal_event(&events, false), None);
        assert_eq!(
            producer_terminal_event(&events, true),
            Some(LocalStop::FocusLost)
        );
        assert_eq!(
            producer_terminal_event(
                &[WindowEvent::FocusPaused, WindowEvent::CloseRequested],
                true
            ),
            Some(LocalStop::WindowClosed)
        );
        let escape = WindowEvent::Key {
            usage: 0x29,
            pressed: true,
            repeat: true,
            captured: None,
            modifiers: (1 << 17) | (1 << 18) | (1 << 19),
            shortcut: None,
        };
        assert_eq!(
            producer_terminal_event(&[WindowEvent::FocusPaused, escape], true),
            Some(LocalStop::LocalEscape)
        );
        assert_eq!(producer_terminal_event(&[], true), None);
    }

    #[test]
    fn automation_waits_for_its_first_picture_before_requiring_current_geometry() {
        let opening = super::super::WindowState {
            picture: (0, 0),
            key: true,
            visible: true,
            geometry_current: false,
        };
        assert_eq!(window_stop(Some(opening), None, false), None);
        assert!(!opening.input_ready(), "startup cannot dispatch input");
        assert_eq!(
            window_stop(Some(opening), None, true),
            Some(LocalStop::PictureChanged)
        );

        let current = super::super::WindowState {
            picture: (1280, 720),
            geometry_current: true,
            ..opening
        };
        assert_eq!(window_stop(Some(current), None, true), None);
        assert!(current.input_ready());
        let stale = super::super::WindowState {
            geometry_current: false,
            ..current
        };
        assert_eq!(
            window_stop(Some(stale), None, true),
            Some(LocalStop::PictureChanged)
        );
        let empty = super::super::WindowState {
            picture: (1280, 0),
            ..current
        };
        assert_eq!(
            window_stop(Some(empty), None, true),
            Some(LocalStop::PictureChanged)
        );
        assert_eq!(
            window_stop(Some(current), Some((1920, 1080)), false),
            Some(LocalStop::PictureChanged),
            "an attached producer must always retain its original geometry"
        );
    }

    #[test]
    fn automation_opening_still_requires_a_live_foreground_window() {
        let opening = super::super::WindowState {
            picture: (0, 0),
            key: true,
            visible: true,
            geometry_current: false,
        };
        assert_eq!(
            window_stop(None, None, false),
            Some(LocalStop::NativeFailure)
        );
        assert_eq!(
            window_stop(
                Some(super::super::WindowState {
                    key: false,
                    ..opening
                }),
                None,
                false,
            ),
            Some(LocalStop::FocusLost)
        );
        assert_eq!(
            window_stop(
                Some(super::super::WindowState {
                    visible: false,
                    ..opening
                }),
                None,
                false,
            ),
            Some(LocalStop::WindowHidden)
        );
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
                shortcut: None,
            },
            WindowEvent::Overflow,
            WindowEvent::FocusLost,
            WindowEvent::Key {
                usage: 0x29,
                pressed: true,
                repeat: true,
                captured: None,
                modifiers: (1 << 17) | (1 << 18) | (1 << 19),
                shortcut: None,
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
