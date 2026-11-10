//! One viewer session on its own thread: frd's reconnecting supervisor over
//! the named (`MagicDNS` + strict `WebPKI`) client, the in-process
//! `VideoToolbox` presenter, and the granted event source the app's input
//! calls push into.
//!
//! The app's main thread never blocks on the session. It polls events,
//! copies the newest picture, reports what it actually drew, and pushes input;
//! each input call takes one short lock and either queues one event in frd's
//! bounded source or refuses. A new key press or text while that source is
//! backed up answers `Busy` (the app sends it again on a later frame, so a
//! paste never overflows it); any other refused push ends control (frd's
//! contract for a native producer); so do closing, backgrounding and an
//! invisible view. Ending control ends the session (`CONTROL_STOPPED` when
//! this device stopped it); viewing again is a new session.
use crate::{
    events::{self, Event, Events, kind, state},
    input::{self, LineScroller},
    video::{Cursor, Decoder, FrameSlot},
};
use asupersync::{
    cx::Cx,
    runtime::RuntimeBuilder,
    time::sleep,
    types::{Budget, CancelKind},
};
use fr_client::input::viewport::SurfaceRect;
use fr_core::input::{DesktopPoint, InputBounds, KeyTransition, ScrollUnit};
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
    native_connection::{
        Client, Configuration, HostName, HostPin, PinWatch,
        reconnect::{self, Application, CallbackError, Failure, Status},
    },
    session_startup::{
        ControlledViewer, InteractiveViewerState, NativeObserver, ObserverError, ObserverPolicy,
        Presentation, Viewer,
        viewer_events::{
            Event as InputEvent, Layout, LocalPoint, NativeCapture, PositionedAction, Source,
        },
    },
    worker::{Deadline, Launch, LocalPresenter},
};
use std::{
    cell::Cell,
    future::{Future, poll_fn},
    ops::ControlFlow,
    pin::pin,
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering},
    },
    task::Poll,
    time::{Duration, Instant},
};

/// Why a session ended (`FR_END_*` in `include/fr_mobile.h`). Content-free.
pub mod end {
    pub const OK: u32 = 0;
    pub const HOST_CLOSED: u32 = 1;
    pub const NAME_UNRESOLVED: u32 = 2;
    pub const OUTSIDE_TAILNET: u32 = 3;
    pub const IDENTITY_CHANGED: u32 = 4;
    pub const TLS_REFUSED: u32 = 5;
    pub const UNREACHABLE: u32 = 6;
    pub const HOST_REFUSED: u32 = 7;
    pub const SESSION_FAILED: u32 = 8;
    pub const CANCELLED: u32 = 9;
    pub const INTERNAL: u32 = 10;
    /// Control stopped on this device (the view left the screen, input was
    /// refused, or the picture changed size), and the session with it.
    pub const CONTROL_STOPPED: u32 = 11;
}

/// Why this device stopped control (`CONTROL_STOPPED`'s detail). The first
/// reason recorded wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
enum LocalStop {
    /// The view left the screen (background, a covering system view).
    Hidden = 1,
    /// frd refused an input push (a closed or failed source).
    InputRefused = 2,
    /// The host's picture changed size, so the attached layout is stale.
    PictureChanged = 3,
}
impl LocalStop {
    const fn from_code(code: u32) -> Option<Self> {
        match code {
            1 => Some(Self::Hidden),
            2 => Some(Self::InputRefused),
            3 => Some(Self::PictureChanged),
            _ => None,
        }
    }
}

/// Input call results (`FR_INPUT_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputResult {
    Sent,
    /// Accepted locally with nothing to send yet (a sub-line scroll).
    Pending,
    NotControlled,
    Outside,
    Unsupported,
    Invalid,
    /// The push failed: control (and with it this session) is ending.
    ControlLost,
    /// A new press or text was not sent because frd's input queue is backed
    /// up ([`PACED_EVENTS`]); send it again on a later frame.
    Busy,
}

/// The control request this app makes: the host's grant profile minus
/// `Repeat` (the app never synthesizes key repeat; held keys stay held).
fn requested_capabilities() -> Capabilities {
    Capabilities::default()
        .with(Capability::Keys)
        .with(Capability::Absolute)
        .with(Capability::Buttons)
        .with(Capability::LineScroll)
}

pub struct Config {
    pub host: HostName,
    pub port: u16,
    pub pin: Option<HostPin>,
    pub control: bool,
    /// Dial `pin`'s own addresses instead of resolving `host`: a machine
    /// listed by a host's tailnet directory (`fr_directory_fetch`). The pin
    /// is required and names `host`; TLS still verifies exactly that name.
    pub direct: bool,
}

/// The granted producer frd owns for this session (see `NativeCapture`).
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
struct Granted {
    source: Source,
    layout: Layout,
    picture: (u32, u32),
    stop: Arc<AtomicBool>,
    released: Arc<AtomicBool>,
    keys: [bool; 256],
    buttons: [bool; 5],
    scroll: LineScroller,
}
impl Drop for Granted {
    fn drop(&mut self) {
        self.source.stop();
        self.released.store(true, Ordering::SeqCst);
    }
}

/// Everything the app's threads and the session thread share.
pub struct Shared {
    pub events: Events,
    pub frames: FrameSlot,
    pub cursor: Mutex<Cursor>,
    granted: Mutex<Option<Granted>>,
    close: AtomicBool,
    want_control: AtomicBool,
    visible: AtomicBool,
    /// How many times the view went from visible to hidden: a control
    /// request made before a hide never completes after it.
    hides: AtomicU64,
    /// [`LocalStop`] code, 0 until this device stopped control.
    local_stop: AtomicU32,
    serving: AtomicBool,
    finished: AtomicBool,
    picture: Mutex<(u32, u32)>,
    full_range: AtomicBool,
    hardware: AtomicI32,
    pin: Mutex<Option<String>>,
    detail: Mutex<Option<String>>,
    end: AtomicU32,
    started: Instant,
    first_view_micros: AtomicU64,
}
impl Shared {
    fn new() -> Self {
        Self {
            events: Events::default(),
            frames: FrameSlot::default(),
            cursor: Mutex::default(),
            granted: Mutex::default(),
            close: AtomicBool::new(false),
            want_control: AtomicBool::new(false),
            visible: AtomicBool::new(false),
            hides: AtomicU64::new(0),
            local_stop: AtomicU32::new(0),
            serving: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            picture: Mutex::new((0, 0)),
            full_range: AtomicBool::new(false),
            hardware: AtomicI32::new(-1),
            pin: Mutex::default(),
            detail: Mutex::default(),
            end: AtomicU32::new(u32::MAX),
            started: Instant::now(),
            first_view_micros: AtomicU64::new(0),
        }
    }
    /// Orderly close: a serving session sends its close; anything earlier
    /// is cancelled by the supervisor. Idempotent.
    pub fn close(&self) {
        self.close.store(true, Ordering::SeqCst);
        self.stop_control(None);
    }
    pub fn finished(&self) -> bool {
        self.finished.load(Ordering::SeqCst)
    }
    /// The view is on screen in an active app (not backgrounded, covered or
    /// detached). Frames count as seen only while this holds; losing it
    /// ends control at once, and a pending control request with it.
    pub fn set_visible(&self, visible: bool) {
        let was = self.visible.swap(visible, Ordering::SeqCst);
        if !visible {
            if was {
                self.hides.fetch_add(1, Ordering::SeqCst);
            }
            self.stop_control(Some(LocalStop::Hidden));
        }
    }
    pub fn request_control(&self) {
        self.want_control.store(true, Ordering::SeqCst);
    }
    /// Ending control ends the session (frd's contract); this asks for the
    /// orderly close and fences input now.
    pub fn release_control(&self) {
        self.want_control.store(false, Ordering::SeqCst);
        self.close();
    }
    pub fn pin(&self) -> Option<String> {
        self.pin.lock().ok()?.clone()
    }
    pub fn detail(&self) -> Option<String> {
        self.detail.lock().ok()?.clone()
    }
    pub fn full_range(&self) -> bool {
        self.full_range.load(Ordering::Relaxed)
    }
    pub fn hardware(&self) -> i32 {
        self.hardware.load(Ordering::Relaxed)
    }
    pub fn first_view_micros(&self) -> u64 {
        self.first_view_micros.load(Ordering::Relaxed)
    }
    /// The `end::*` code once the session ended, else `u32::MAX`.
    pub fn end_code(&self) -> u32 {
        self.end.load(Ordering::SeqCst)
    }
    pub fn controlled(&self) -> bool {
        self.granted.lock().is_ok_and(|g| g.is_some())
    }
    /// Fence the granted source now (focus loss, backgrounding, close).
    /// `reason`: this device stopped a live grant (not a close it asked
    /// for, and not the attempt's own cleanup).
    fn stop_control(&self, reason: Option<LocalStop>) {
        let dropped = match self.granted.lock() {
            Ok(mut granted) => granted.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
        if dropped.is_some() {
            if let Some(reason) = reason {
                self.note_local_stop(reason);
            }
            self.events.state(state::CONTROL_ENDED, 0);
        }
        drop(dropped);
    }
    fn note_local_stop(&self, reason: LocalStop) {
        let _ =
            self.local_stop
                .compare_exchange(0, reason as u32, Ordering::SeqCst, Ordering::SeqCst);
    }

    fn with_granted(&self, act: impl FnOnce(&mut Granted) -> InputResult) -> InputResult {
        let Ok(mut slot) = self.granted.lock() else {
            return InputResult::ControlLost;
        };
        let Some(granted) = slot.as_mut() else {
            return InputResult::NotControlled;
        };
        if granted.stop.load(Ordering::SeqCst) {
            drop(slot.take());
            self.events.state(state::CONTROL_ENDED, 0);
            return InputResult::ControlLost;
        }
        let result = act(granted);
        if result == InputResult::ControlLost {
            drop(slot.take());
            self.note_local_stop(LocalStop::InputRefused);
            self.events.state(state::CONTROL_ENDED, 0);
        }
        result
    }

    /// Absolute pointer at encoded picture pixel `(x, y)`.
    pub fn pointer(&self, x: f64, y: f64) -> InputResult {
        self.with_granted(|g| {
            if !g.source.capabilities().contains(Capability::Absolute) {
                return InputResult::Unsupported;
            }
            let Some((sx, sy)) = input::subpixel_point(g.picture, x, y) else {
                return InputResult::Outside;
            };
            let event = InputEvent::Pointer(g.layout.at(LocalPoint::subpixels(sx, sy)));
            push(g, event)
        })
    }
    /// A button transition at encoded picture pixel `(x, y)`. A press of a
    /// held button or a release of an unheld one sends nothing.
    pub fn button(&self, index: u32, pressed: bool, x: f64, y: f64) -> InputResult {
        let Some((slot, button)) = input::button(index) else {
            return InputResult::Invalid;
        };
        self.with_granted(|g| {
            if !g.source.capabilities().contains(Capability::Buttons) {
                return InputResult::Unsupported;
            }
            let Some((sx, sy)) = input::subpixel_point(g.picture, x, y) else {
                return InputResult::Outside;
            };
            if g.buttons[slot] == pressed {
                return InputResult::Pending;
            }
            let event = InputEvent::Positioned {
                location: g.layout.at(LocalPoint::subpixels(sx, sy)),
                action: PositionedAction::Button { button, pressed },
            };
            let result = push(g, event);
            if result == InputResult::Sent {
                g.buttons[slot] = pressed;
            }
            result
        })
    }
    /// Scroll by `dx`, `dy` lines (fractional; positive y scrolls down) at
    /// encoded picture pixel `(x, y)`; whole lines are sent, the rest kept.
    pub fn scroll(&self, dx: f64, dy: f64, x: f64, y: f64) -> InputResult {
        self.with_granted(|g| {
            if !g.source.capabilities().contains(Capability::LineScroll) {
                return InputResult::Unsupported;
            }
            let Some((sx, sy)) = input::subpixel_point(g.picture, x, y) else {
                return InputResult::Outside;
            };
            let Some((lx, ly)) = g.scroll.add(dx, dy) else {
                return InputResult::Pending;
            };
            let event = InputEvent::Positioned {
                location: g.layout.at(LocalPoint::subpixels(sx, sy)),
                action: PositionedAction::Scroll {
                    x: lx,
                    y: ly,
                    unit: ScrollUnit::Lines,
                },
            };
            push(g, event)
        })
    }
    /// Forget a scroll gesture's fractional remainder.
    pub fn scroll_end(&self) {
        let _ = self.with_granted(|g| {
            g.scroll.reset();
            InputResult::Pending
        });
    }
    /// A HID keyboard usage press or release. No repeat is ever sent: a
    /// press of a held key and a release of an unheld key send nothing.
    pub fn key(&self, usage: u16, pressed: bool) -> InputResult {
        let Some((slot, key)) = input::key(usage) else {
            return InputResult::Invalid;
        };
        self.with_granted(|g| {
            if !g.source.capabilities().contains(Capability::Keys) {
                return InputResult::Unsupported;
            }
            if g.keys[slot] == pressed {
                return InputResult::Pending;
            }
            if pressed && paced(g) {
                return InputResult::Busy;
            }
            let transition = if pressed {
                KeyTransition::Press
            } else {
                KeyTransition::Release
            };
            let result = push(g, InputEvent::Key { key, transition });
            if result == InputResult::Sent {
                g.keys[slot] = pressed;
            }
            result
        })
    }
    /// Committed Unicode text, only where the host granted `Text`.
    pub fn text(&self, text: &str) -> InputResult {
        self.with_granted(|g| {
            if !g.source.supports_text() {
                return InputResult::Unsupported;
            }
            if paced(g) {
                return InputResult::Busy;
            }
            let Ok(sampled) = g.source.clock() else {
                return InputResult::ControlLost;
            };
            match g.source.commit_text(text, sampled) {
                Ok(()) => InputResult::Sent,
                Err(frd::session_startup::viewer_events::Error::UnsupportedText) => {
                    InputResult::Unsupported
                }
                Err(_) => InputResult::ControlLost,
            }
        })
    }
    /// Release every key this session pressed (keyboard hidden, focus
    /// lost). Buttons are the gesture code's to release; frd's held-state
    /// cleanup also releases anything held when control ends. Returns the
    /// first failure, if any.
    pub fn release_all(&self) -> InputResult {
        self.with_granted(|g| {
            for slot in 0..256_usize {
                if g.keys[slot] {
                    let Ok(usage) = u16::try_from(slot) else {
                        continue;
                    };
                    let Some((_, key)) = input::key(usage) else {
                        continue;
                    };
                    let result = push(
                        g,
                        InputEvent::Key {
                            key,
                            transition: KeyTransition::Release,
                        },
                    );
                    if result != InputResult::Sent {
                        return result;
                    }
                    g.keys[slot] = false;
                }
            }
            InputResult::Sent
        })
    }
}

/// New key presses and text wait (`Busy`) while frd's captured-event queue
/// holds this many events. A pasted string typed as keys arrives faster than
/// one dispatch per event drains it; the queue's bound (`MAX_EVENTS`) is
/// terminal, and a press older than `MAX_EVENT_AGE_US` is dropped, so a
/// short queue keeps every press young. The app retries every frame, which
/// still types hundreds of characters a second. Releases always go: they
/// only follow presses that went.
const PACED_EVENTS: usize = frd::session_startup::viewer_events::MAX_EVENTS / 8;
fn paced(g: &Granted) -> bool {
    g.source
        .queued()
        .is_some_and(|queued| queued >= PACED_EVENTS)
}

fn push(g: &mut Granted, event: InputEvent) -> InputResult {
    let Ok(sampled) = g.source.clock() else {
        return InputResult::ControlLost;
    };
    match g.source.push(event, sampled) {
        Ok(()) => InputResult::Sent,
        Err(_) => InputResult::ControlLost,
    }
}

/// The session handle the C ABI owns.
pub struct Session {
    shared: Arc<Shared>,
    control: bool,
}
impl Session {
    pub fn start(config: Config) -> std::io::Result<Self> {
        let shared = Arc::new(Shared::new());
        let worker = shared.clone();
        let control = config.control;
        // Detached: the thread owns its share of the state and ends on its
        // own (closing in order when asked); nothing ever joins it from the
        // app's thread.
        std::thread::Builder::new()
            .name("fr-session".into())
            .stack_size(4 * 1024 * 1024)
            .spawn(move || {
                // A debug build unwinds: still end exactly once, ENDED last.
                let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run(&worker, config);
                }));
                if ran.is_err() && !worker.finished() {
                    worker.frames.clear();
                    finish(&worker, end::INTERNAL, Some("Panic".into()));
                }
            })?;
        Ok(Self { shared, control })
    }
    pub fn shared(&self) -> &Shared {
        &self.shared
    }
    /// Whether this session was started control-capable.
    pub const fn control(&self) -> bool {
        self.control
    }
}
impl Drop for Session {
    /// Close: the detached thread finishes its orderly close (bounded by
    /// frd's cleanup deadlines). The app's thread never blocks here.
    fn drop(&mut self) {
        self.shared.close();
    }
}

fn finish(shared: &Shared, code: u32, detail: Option<String>) {
    shared.stop_control(None);
    if let Ok(mut slot) = shared.detail.lock() {
        *slot = detail;
    }
    shared.end.store(code, Ordering::SeqCst);
    shared.events.push(Event {
        kind: kind::ENDED,
        value: code,
        ..Event::default()
    });
    shared.finished.store(true, Ordering::SeqCst);
}

fn run(shared: &Arc<Shared>, config: Config) {
    let Ok(runtime) = RuntimeBuilder::current_thread()
        .enable_platform_reactor(true)
        .build()
    else {
        return finish(shared, end::INTERNAL, Some("MissingRuntime".into()));
    };
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    // Independently live while cancellation drains the original connection:
    // the granted controller's closing exchange runs on it.
    let Ok(local) = runtime
        .handle()
        .try_request_cx_with_budget(Budget::INFINITE)
    else {
        return finish(shared, end::INTERNAL, Some("MissingRuntime".into()));
    };
    let Ok(roots) = crate::roots::bundled() else {
        return finish(shared, end::INTERNAL, Some("InvalidTrustStore".into()));
    };
    let control = config.control;
    let client = match (config.direct, config.pin) {
        (true, Some(pin)) if pin.name() == &config.host => {
            Client::named_at(pin, roots, Duration::from_secs(5))
        }
        (true, _) => return finish(shared, end::INTERNAL, Some("InvalidConfiguration".into())),
        (false, pin) => Client::named(config.host, pin, roots, Duration::from_secs(5)),
    };
    let Ok(mut client) = client else {
        return finish(shared, end::INTERNAL, Some("InvalidConfiguration".into()));
    };
    let Some(watch) = client.pin_watch() else {
        return finish(shared, end::INTERNAL, Some("InvalidConfiguration".into()));
    };
    let mut epoch = [0_u8; 16];
    cx.random_bytes(&mut epoch);
    let epoch = u128::from_be_bytes(epoch).max(1);
    let mut app = Mobile::new(shared.clone(), control, local, epoch, watch);
    let cfg = Configuration {
        port: config.port,
        ..Configuration::default()
    };
    let policy = reconnect::Policy {
        max_attempts: 3,
        ..reconnect::Policy::default()
    };
    let result = if control {
        let operation = client.run_named_control_capable(
            cx.clone(),
            runtime.handle(),
            cfg,
            fr_client::native::control_offer(),
            policy,
            &mut app,
        );
        runtime.block_on(supervise(&cx, shared, operation))
    } else {
        let operation = client.run_named_observing(
            cx.clone(),
            runtime.handle(),
            cfg,
            fr_client::native::observation_offer(),
            policy,
            &mut app,
        );
        runtime.block_on(supervise(&cx, shared, operation))
    };
    publish_pin(shared, client.pin_watch().as_ref());
    let (code, detail) = ending(
        result,
        shared.close.load(Ordering::SeqCst),
        LocalStop::from_code(shared.local_stop.load(Ordering::SeqCst)),
        app.terminal_failure(),
    );
    shared.frames.clear();
    finish(shared, code, detail);
}

/// The end code and detail for a finished run. `closed`: this app asked
/// for the close; `stopped`: this device stopped control first.
fn ending(
    result: Result<(), Failure>,
    closed: bool,
    stopped: Option<LocalStop>,
    terminal: Option<ObserverError>,
) -> (u32, Option<String>) {
    match (result, stopped) {
        (Ok(()), _) if closed => (end::OK, None),
        // This device stopped control, and so ended the session in order
        // (or its close then failed): never "the host closed".
        (Ok(()) | Err(Failure::Observation(_)), Some(reason)) => {
            (end::CONTROL_STOPPED, Some(format!("{reason:?}")))
        }
        (Ok(()), None) => (end::HOST_CLOSED, None),
        (Err(failure), _) => (
            classify(failure, closed),
            Some(format!(
                "{:?}",
                terminal.map_or(failure, Failure::Observation)
            )),
        ),
    }
}

/// Content-free classification for the app's message. The exact typed
/// failure stays available as `fr_session_copy_detail`.
fn classify(failure: Failure, closed: bool) -> u32 {
    use frd::native_connection::Error as C;
    // Cancellation or a served session's end after this app's own close is
    // the orderly end it asked for. A refused connection keeps its reason.
    // A close during a reconnect makes the supervisor's status callback
    // refuse to go on (`Notification`): also the end this app asked for.
    if closed
        && matches!(
            failure,
            Failure::Cancelled
                | Failure::Notification
                | Failure::Observation(_)
                | Failure::Connection(C::Cancelled)
        )
    {
        return end::OK;
    }
    match failure {
        Failure::Cancelled => end::CANCELLED,
        Failure::Connection(C::Tailnet(error)) => tailnet_end(error),
        Failure::Connection(C::Session(_)) => end::HOST_REFUSED,
        Failure::Observation(_) => end::SESSION_FAILED,
        _ => end::INTERNAL,
    }
}

/// The app's end code for a failed named connection (sessions and the
/// directory fetch alike).
pub(crate) const fn tailnet_end(error: fr_tailnet::Error) -> u32 {
    use fr_tailnet::Error as T;
    match error {
        T::NameUnresolved => end::NAME_UNRESOLVED,
        T::OutsideTailnet => end::OUTSIDE_TAILNET,
        T::IdentityChanged => end::IDENTITY_CHANGED,
        // The pinned stack reports a refused server certificate as a
        // failed native handshake (fr-tailnet's named tests).
        T::NativeHandshake | T::PeerCertificateRejected => end::TLS_REFUSED,
        T::HandshakeTimeout | T::NativeRouteIo | T::NativeBind | T::Timeout => end::UNREACHABLE,
        _ => end::SESSION_FAILED,
    }
}

fn publish_pin(shared: &Shared, watch: Option<&PinWatch>) {
    let Some(pin) = watch.and_then(PinWatch::get) else {
        return;
    };
    let text = pin.to_string();
    let changed = match shared.pin.lock() {
        Ok(mut slot) if slot.as_deref() != Some(text.as_str()) => {
            *slot = Some(text);
            true
        }
        _ => false,
    };
    if changed {
        shared.events.push(Event {
            kind: kind::PIN,
            ..Event::default()
        });
    }
}

/// Drive the supervisor. A close request before the session serves cancels
/// it outright (nothing remote to close yet); a serving session closes in
/// order from its UI callback, and is cancelled if that takes too long.
async fn supervise<T>(
    cx: &Cx,
    shared: &Shared,
    operation: impl Future<Output = Result<T, Failure>>,
) -> Result<T, Failure> {
    const TURN: Duration = Duration::from_millis(10);
    const ORDERLY: Duration = Duration::from_secs(3);
    let mut operation = pin!(operation);
    let timer = cx.timer_driver();
    let mut tick = None;
    let mut closing_since: Option<Instant> = None;
    poll_fn(move |task| {
        loop {
            if let Poll::Ready(result) = operation.as_mut().poll(task) {
                return Poll::Ready(result);
            }
            if shared.close.load(Ordering::SeqCst) {
                let since = *closing_since.get_or_insert_with(Instant::now);
                if !shared.serving.load(Ordering::SeqCst) || since.elapsed() > ORDERLY {
                    cx.cancel_fast(CancelKind::User);
                }
            }
            let Some(timer) = &timer else {
                return Poll::Pending;
            };
            let next = tick.get_or_insert_with(|| Box::pin(sleep(timer.now(), TURN)));
            if next.as_mut().poll(task).is_pending() {
                return Poll::Pending;
            }
            tick = None;
        }
    })
    .await
}

/// The in-process presenter frd's worker drives on its blocking pool.
struct Presenter {
    shared: Arc<Shared>,
    decoder: Option<Decoder>,
}
fn worker_error(error: crate::video::Error) -> WorkerError {
    use crate::video::Error as E;
    match error {
        E::InvalidConfiguration | E::UnsupportedBitstream => WorkerError::Unsupported,
        E::StaleGeneration => WorkerError::WrongEpoch,
        E::Allocation => WorkerError::Allocation,
        E::Geometry => WorkerError::GeometryChanged,
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
        let decoder = Decoder::new(
            configuration.codec()?,
            configuration.limits()?,
            record.bytes(),
        )
        .map_err(worker_error)?;
        let picture = decoder.picture();
        self.shared
            .hardware
            .store(decoder.hardware().map_or(-1, i32::from), Ordering::Relaxed);
        self.shared
            .full_range
            .store(decoder.full_range(), Ordering::Relaxed);
        // A changed picture size invalidates any attached layout: control
        // ends (frd also refuses the old layout).
        let previous = self
            .shared
            .picture
            .lock()
            .map(|mut p| std::mem::replace(&mut *p, picture))
            .unwrap_or_default();
        if previous != (0, 0) && previous != picture {
            self.shared.stop_control(Some(LocalStop::PictureChanged));
        }
        self.shared.events.push(Event {
            kind: kind::VIDEO,
            picture_width: picture.0,
            picture_height: picture.1,
            ..Event::default()
        });
        self.decoder = Some(decoder);
        Ok(())
    }
    fn submit(&mut self, unit: &EncodedAccessUnit, present: bool) -> Result<(), WorkerError> {
        let decoder = self.decoder.as_mut().ok_or(WorkerError::WrongState)?;
        let started = Instant::now();
        let image = decoder.submit(unit, present).map_err(worker_error)?;
        let micros = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
        self.shared.frames.record_decode(micros);
        if let Some(image) = image {
            self.shared.frames.publish(image, unit.frame().as_raw());
        }
        Ok(())
    }
    fn overlay(&mut self, update: &CursorUpdate<'_>) -> Result<(), WorkerError> {
        self.shared
            .cursor
            .lock()
            .map_err(|_| WorkerError::NativeFailure)?
            .apply(update)
            .map_err(worker_error)
    }
}

/// A presented frame counts as seen once the app has drawn it (or a later
/// one) into a presented drawable and the view is visible.
/// frd hands a submitted picture to the session callback before the app's
/// Metal pass has drawn it, then calls again with `None` after every network
/// turn. Keep the newest submitted picture pending, even while the view is
/// hidden (an idle host may send nothing newer once it is back), until the
/// app reports it (or a later one) drawn while visible, and report it once.
/// A newer submission replaces it: frd's view tracker accepts visibility of
/// its newest picture only. Drawn ids carry the attempt's generation
/// ([`FrameSlot::app_id`]), so an earlier attempt's drawing never counts.
fn shown(shared: &Shared, pending: &mut Option<u64>, frame: Option<Presentation>) -> Option<u64> {
    if let Some(frame) = frame {
        *pending =
            (frame.stage == PresentationStage::SubmittedToCompositor).then(|| frame.frame.as_raw());
    }
    let id = (*pending)?;
    if !shared.visible.load(Ordering::SeqCst) || shared.frames.drawn() < shared.frames.app_id(id) {
        return None;
    }
    *pending = None;
    Some(id)
}

/// The primary display: the one at the desktop origin, else the first.
#[allow(clippy::unnecessary_wraps)] // frd's display selector signature
fn choose_display(catalog: &Catalog) -> Result<Option<u128>, ()> {
    let displays = catalog.displays();
    Ok(displays
        .iter()
        .find(|d| d.x == 0 && d.y == 0)
        .or_else(|| displays.first())
        .map(|d| d.handle))
}

/// What attaching the granted source did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Attach {
    /// No picture yet, or the view is not visible: try again next turn.
    NotYet,
    Installed,
    /// The view hid or the app closed while attaching: the grant was fenced
    /// before any input could use it.
    Fenced,
}

/// Attach the granted source with a layout that maps the whole encoded
/// picture onto the selected display. `hides` is the hide count when control
/// was requested: a hide since then (even one racing this attach) fences.
fn attach(
    viewer: &mut ControlledViewer,
    display: Display,
    shared: &Shared,
    hides: u64,
) -> Result<Attach, ()> {
    let picture = shared.picture.lock().map(|p| *p).map_err(|_| ())?;
    if picture.0 == 0 || picture.1 == 0 || !shared.visible.load(Ordering::SeqCst) {
        return Ok(Attach::NotYet);
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
    let area = SurfaceRect::new(0, 0, picture.0, picture.1).map_err(|_| ())?;
    let layout = viewer.configure_viewport(source, area).map_err(|_| ())?;
    viewer.confirm_viewport(&layout).map_err(|_| ())?;
    let stop = Arc::new(AtomicBool::new(false));
    let released = Arc::new(AtomicBool::new(false));
    let capture = Capture {
        stop: stop.clone(),
        released: released.clone(),
    };
    let mut outcome = Attach::Fenced;
    viewer
        .capture_input_owned(&layout.clone(), |source| {
            let Ok(mut slot) = shared.granted.lock() else {
                return Err(());
            };
            let granted = Granted {
                source,
                layout,
                picture,
                stop,
                released,
                keys: [false; 256],
                buttons: [false; 5],
                scroll: LineScroller::default(),
            };
            // Re-checked under the lock `set_visible` and `close` fence
            // through: one that ran after the check above found nothing to
            // fence, so this grant must not outlive it. Dropping it stops
            // the source and reports it released.
            if shared.visible.load(Ordering::SeqCst)
                && !shared.close.load(Ordering::SeqCst)
                && shared.hides.load(Ordering::SeqCst) == hides
            {
                *slot = Some(granted);
                outcome = Attach::Installed;
            } else {
                drop(granted);
            }
            Ok(capture)
        })
        .map_err(|_| ())?;
    Ok(outcome)
}

/// frd's per-attempt application: open the presenter after approval and
/// display selection, serve until closed, and clean up every native owner.
struct Mobile {
    shared: Arc<Shared>,
    control: bool,
    close_cx: Cx,
    epoch: u128,
    watch: PinWatch,
    observer: Option<NativeObserver>,
    started: bool,
    native_started: Rc<Cell<bool>>,
    service_succeeded: bool,
    terminal: Option<ObserverError>,
}
impl Mobile {
    fn new(shared: Arc<Shared>, control: bool, close_cx: Cx, epoch: u128, watch: PinWatch) -> Self {
        Self {
            shared,
            control,
            close_cx,
            epoch,
            watch,
            observer: None,
            started: false,
            native_started: Rc::default(),
            service_succeeded: false,
            terminal: None,
        }
    }
}

impl Application for Mobile {
    type Output = ();
    #[allow(clippy::too_many_lines)] // one attempt: open, serve, control
    async fn run(&mut self, attempt: u8, viewer: Viewer) -> Result<(), ObserverError> {
        if self.started || self.native_started.get() || self.observer.is_some() {
            return Err(ObserverError::Order);
        }
        self.started = true;
        self.service_succeeded = false;
        self.shared.frames.begin_attempt();
        publish_pin(&self.shared, Some(&self.watch));
        let attempt_number = u32::from(attempt);
        let epoch = self
            .epoch
            .checked_add(u128::from(attempt))
            .filter(|epoch| *epoch != 0)
            .ok_or(ObserverError::InvalidConfiguration)?;
        let shared = self.shared.clone();
        let native_started = self.native_started.clone();
        let approvals = self.shared.clone();
        let opening = viewer.observe_with_renderer(
            move |display, _original| async move {
                native_started.set(true);
                shared.events.push(Event {
                    kind: kind::VIDEO,
                    display_width: display.pixel_width,
                    display_height: display.pixel_height,
                    ..Event::default()
                });
                Launch::in_process(
                    Box::new(Presenter {
                        shared,
                        decoder: None,
                    }),
                    epoch,
                )
                .map_err(|_| ())
            },
            ObserverPolicy::default(),
            self.control.then(ClockPolicy::default),
            choose_display,
            move |_notice| {
                approvals
                    .events
                    .state(state::AWAITING_APPROVAL, attempt_number);
                Ok(())
            },
        );
        let opened = opening.await?;
        let display = opened.display();
        let observer = self.observer.insert(opened);
        self.shared.serving.store(true, Ordering::SeqCst);
        let shared = self.shared.clone();
        let mut viewing = false;
        let mut announce_view = move |shared: &Shared| {
            if !viewing {
                viewing = true;
                let micros = u64::try_from(shared.started.elapsed().as_micros()).unwrap_or(0);
                let _ = shared.first_view_micros.compare_exchange(
                    0,
                    micros.max(1),
                    Ordering::AcqRel,
                    Ordering::Acquire,
                );
                shared.events.state(state::VIEWING, attempt_number);
            }
        };
        let mut pending = None;
        let outcome = if self.control {
            let mut mapped = false;
            let mut last_shown = None;
            let mut attached = false;
            let mut requested = false;
            let mut hides_at_request = 0;
            let mut announced_control = false;
            let ui = move |state: InteractiveViewerState<'_>, frame: Option<Presentation>| {
                if shared.close.load(Ordering::SeqCst) {
                    shared.events.state(state::CLOSING, attempt_number);
                    return Ok(ControlFlow::Break(fr_wire::closure::Reason::Requested));
                }
                let fresh = shown(&shared, &mut pending, frame).filter(|f| last_shown != Some(*f));
                match state {
                    InteractiveViewerState::Viewing(view) => {
                        let Some(frame) = fresh.or(last_shown) else {
                            return Ok(ControlFlow::Continue(()));
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
                        announce_view(&shared);
                        if !requested
                            && shared.want_control.load(Ordering::SeqCst)
                            && shared.visible.load(Ordering::SeqCst)
                            && view.evidence().is_ok()
                        {
                            shared.want_control.store(false, Ordering::SeqCst);
                            requested = true;
                            hides_at_request = shared.hides.load(Ordering::SeqCst);
                            view.request_control().map_err(|_| ())?;
                            shared
                                .events
                                .state(state::REQUESTING_CONTROL, attempt_number);
                        }
                    }
                    InteractiveViewerState::Requesting(pending) => {
                        if shared.hides.load(Ordering::SeqCst) != hides_at_request {
                            // Hidden while asking: the grant must not attach
                            // when the view returns (no automatic
                            // reacquisition), so the request ends the session.
                            shared.note_local_stop(LocalStop::Hidden);
                            return Ok(ControlFlow::Break(fr_wire::closure::Reason::Requested));
                        }
                        if let Some(frame) = fresh
                            && pending.visible(frame).is_ok()
                        {
                            last_shown = Some(frame);
                        }
                    }
                    InteractiveViewerState::Controlled(viewer) => {
                        if !requested {
                            return Err(()); // no local request on this owner
                        }
                        if let Some(frame) = fresh {
                            let _ = viewer.visible(frame);
                            last_shown = Some(frame);
                        }
                        if !attached {
                            match attach(viewer, display, &shared, hides_at_request)? {
                                Attach::NotYet
                                    if shared.hides.load(Ordering::SeqCst) == hides_at_request => {}
                                Attach::NotYet | Attach::Fenced => {
                                    if !shared.close.load(Ordering::SeqCst) {
                                        shared.note_local_stop(LocalStop::Hidden);
                                    }
                                    return Ok(ControlFlow::Break(
                                        fr_wire::closure::Reason::Requested,
                                    ));
                                }
                                Attach::Installed => {
                                    attached = true;
                                    if !announced_control {
                                        announced_control = true;
                                        shared.events.state(state::CONTROLLED, attempt_number);
                                    }
                                }
                            }
                        } else if !shared.controlled() {
                            // The local source was fenced (hidden view, a
                            // refused push): control, and so the session, ends.
                            return Ok(ControlFlow::Break(fr_wire::closure::Reason::Requested));
                        }
                    }
                }
                Ok(ControlFlow::Continue(()))
            };
            observer
                .serve_interactive_session_until(
                    &self.close_cx,
                    1,
                    requested_capabilities(),
                    fr_client::input::Policy::default(),
                    ui,
                    |_result| {},
                )
                .await
                .map(|_| ())
        } else {
            observer
                .serve_until(move |frame| {
                    if shown(&shared, &mut pending, frame).is_some() {
                        announce_view(&shared);
                    }
                    Ok(if shared.close.load(Ordering::SeqCst) {
                        shared.events.state(state::CLOSING, attempt_number);
                        ControlFlow::Break(fr_wire::closure::Reason::Requested)
                    } else {
                        ControlFlow::Continue(())
                    })
                })
                .await
                .map(|_| ())
                .map_err(ObserverError::Streaming)
        };
        self.shared.serving.store(false, Ordering::SeqCst);
        self.service_succeeded = outcome.is_ok();
        outcome
    }
    fn cleanup(
        &mut self,
        cx: &Cx,
        deadline: Deadline,
    ) -> impl Future<Output = Result<(), CallbackError>> {
        // Fence at call time: no input outlives the attempt.
        self.shared.stop_control(None);
        self.shared.serving.store(false, Ordering::SeqCst);
        async move {
            let result = match self.observer.as_mut() {
                Some(observer) => {
                    self.terminal = self.terminal.or_else(|| reported_terminal(observer));
                    observer.close();
                    let media = observer.reap_media(cx, deadline).await;
                    let input = observer.reap_input_capture(cx, deadline).await;
                    let files = observer.reap_files(cx, deadline).await;
                    media.is_ok() && input.is_ok() && files.is_ok()
                }
                // Approval and display selection precede the deferred native
                // factory; once it started, losing it proves no cleanup.
                None => !self.native_started.get(),
            };
            if !result {
                return Err(CallbackError);
            }
            self.observer = None;
            self.started = false;
            self.native_started.set(false);
            Ok(())
        }
    }
    fn status(&mut self, status: Status) -> Result<(), CallbackError> {
        let closing = self.shared.close.load(Ordering::SeqCst);
        match status {
            Status::Connecting { attempt } => {
                if self.started || self.native_started.get() || self.observer.is_some() {
                    return Err(CallbackError);
                }
                if closing {
                    return Err(CallbackError);
                }
                self.terminal = None;
                self.service_succeeded = false;
                self.shared
                    .events
                    .state(state::CONNECTING, u32::from(attempt));
            }
            Status::Authenticated { attempt } => {
                publish_pin(&self.shared, Some(&self.watch));
                self.shared
                    .events
                    .state(state::AUTHENTICATED, u32::from(attempt));
            }
            Status::Reconnecting { attempt, .. } | Status::Reopening { attempt } => {
                if closing {
                    return Err(CallbackError);
                }
                self.shared
                    .events
                    .state(state::RECONNECTING, u32::from(attempt));
            }
            Status::Cleaning { .. } | Status::Completed { .. } | Status::Stopped { .. } => {}
        }
        Ok(())
    }
    fn terminal_failure(&self) -> Option<ObserverError> {
        self.terminal
            .filter(|error| !self.service_succeeded || !requested_terminal(*error))
    }
}

/// An expected close the client itself asked for (a released lease or a
/// session it closed), not an adverse report. Mirrors fr-native's macOS
/// session.
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
/// remove it (fr-native's macOS session's rule). Local close, media exit and
/// source release create no host proof.
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

/// Unused re-export guard: the event module's constants are the ABI.
pub use events::{kind as event_kind, state as event_state};

#[cfg(test)]
mod tests;
