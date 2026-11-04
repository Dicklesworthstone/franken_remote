//! X11 logical input from an explicitly selected local renderer window into the
//! original granted viewer's bounded queue. An explicit connection-time mode
//! can grab the keyboard on that same focused local window for system shortcuts.
//! No root/raw capture, key injection, text/IME guessing, window creation or
//! replacement authority. The default mode leaves system shortcuts local.
//! Native work has one thread/connection owner, separate from codecs and QUIC.
//! Requires XI2 and exactly one enabled master pointer/keyboard pair. Held input
//! is sampled at most four times per second, never adopted from another device.
//! No native queries are issued while nothing is held and no events arrive.
//! Physical Ctrl+Alt+Shift+Escape is always a local stop, including when remote
//! keyboard input was not negotiated. Its entire native batch is withheld. The
//! host agent still owns release of previously submitted remote modifiers.
pub(crate) mod display_switch;
mod escape;
mod file_keys;
mod held;
mod status;
mod system_shortcuts;
#[cfg(test)]
mod tests;
mod timing;
use fr_core::{
    input::{KeyTransition, PointerButton, ScrollUnit},
    input_submission::{Capabilities, Capability, scroll::LINE},
};
use frd::session_startup::{
    ViewerControl,
    viewer_events::{ClientInstant, Event, Layout, LocalPoint, PositionedAction, Source},
};
use status::CaptureState;
use std::{
    ffi::{CString, c_char, c_int, c_void},
    fmt,
    ptr::NonNull,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU8, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};
use timing::Timeline;

const MAX_NATIVE_EVENTS: usize = 64;
const NATIVE_TURN_US: u64 = 50_000;
const TURN: Duration = Duration::from_millis(1);
/// `fr_viewer_input_open` refusals (`viewer_input.c`); anything else is native.
const OPEN_NOT_FOCUSED: c_int = 1;
const OPEN_HELD: c_int = 2;
const OPEN_WINDOW: c_int = 3;
/// A granted controller's window may map without keyboard focus (a window
/// manager's focus-stealing prevention, or no click yet). Nothing is captured
/// until it is focused; this bounds how long control waits for that, after
/// which the attempt ends as `FocusLost` instead of a generic native failure.
const FOCUS_WAIT: Duration = Duration::from_secs(30);
const FOCUS_POLL: Duration = Duration::from_millis(50);
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Raw {
    kind: u32,
    time: u32,
    detail: u32,
    x: i32,
    y: i32,
}
unsafe extern "C" {
    fn fr_viewer_input_open(
        display: *const c_char,
        window: u32,
        width: u32,
        height: u32,
        names: *mut [[u8; 4]; 256],
        refusal: *mut c_int,
    ) -> *mut c_void;
    fn fr_viewer_input_close(handle: *mut c_void);
    fn fr_viewer_input_barrier(handle: *mut c_void) -> c_int;
    fn fr_viewer_input_next(handle: *mut c_void, event: *mut Raw) -> c_int;
    fn fr_viewer_input_display_switch(handle: *mut c_void, enabled: c_int) -> c_int;
}

/// Physical window size is fixed for this capture generation. Configure and
/// confirm the renderer's Layout separately on the original `ControlledViewer`.
/// A resize/focus loss requires a new explicit control acquisition, not resume.
#[derive(Clone, Copy)]
pub struct Window {
    pub id: u32,
    pub width: u32,
    pub height: u32,
}
impl Window {
    fn valid(self, layout: &Layout) -> bool {
        let rect = layout.destination();
        self.id != 0
            && (1..=32767).contains(&self.width)
            && (1..=32767).contains(&self.height)
            && rect.origin().x >= 0
            && rect.origin().y >= 0
            && i64::from(rect.origin().x) + i64::from(rect.width()) <= i64::from(self.width)
            && i64::from(rect.origin().y) + i64::from(rect.height()) <= i64::from(self.height)
    }
}
/// Fixed categories only: no captured keys, text, coordinates or display strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum StopReason {
    Local = 2,
    Closed,
    NativeFailure,
    FocusLost,
    WindowChanged,
    KeymapChanged,
    SyntheticInput,
    Overflow,
    Expired,
    Clock,
    UnsupportedKey,
    InitialHeld,
    Queue,
    InputDevicesChanged,
    EscapeUnavailable,
    LocalEscape,
    SystemShortcutsUnavailable,
    /// A real `ConfigureNotify` changed this original target's dimensions.
    /// Separate from hide, unmap, obscuring and unverified window changes.
    WindowResized,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Opening,
    Running,
    Stopped(StopReason),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    InvalidWindow,
    InvalidDisplay,
    Closed,
    ThreadUnavailable,
    /// Native initialization ended the original input owner before attachment
    /// returned. The controlled viewer still retains its native cleanup owner.
    Stopped(StopReason),
}
struct Shared {
    control: ViewerControl,
    state: CaptureState,
    files: OnceLock<frd::native_files::SendControl>,
    file_keymap: AtomicU8,
    system_shortcuts: bool,
    #[cfg(feature = "linux-viewer-window")]
    window: Window,
    #[cfg(feature = "linux-viewer-window")]
    display_switch: OnceLock<crate::viewer_window::WindowControl>,
}
impl Shared {
    fn stop(&self, reason: StopReason) {
        #[cfg(feature = "linux-viewer-window")]
        if self.state.first_stop().is_none()
            && !matches!(
                reason,
                StopReason::Local | StopReason::Closed | StopReason::WindowResized
            )
            && let Some(window) = self.display_switch.get()
        {
            window.cancel_display_switch();
            window.cancel_resize();
        }
        // Reserve the diagnostic cause before the fence wakes session cleanup.
        // Publish Stopped only after the original viewer has actually fenced.
        self.state.stop(reason, || self.control.stop());
    }
}
#[derive(Clone)]
pub struct CaptureControl(Arc<Shared>);
impl CaptureControl {
    pub fn stop(&self) {
        // An explicit local stop vetoes reopening even when resize won the
        // original diagnostic race. Native cleanup uses retire() below.
        #[cfg(feature = "linux-viewer-window")]
        if let Some(window) = self.0.display_switch.get() {
            window.cancel_resize();
        }
        self.retire();
    }
    fn retire(&self) {
        self.0.stop(StopReason::Local);
    }
    pub fn status(&self) -> Status {
        state(self.0.state.load())
    }
    /// Bind the original capture to its exact local renderer's one-use switch
    /// intent. No peer data, replacement window or second input owner is used.
    #[cfg(feature = "linux-viewer-window")]
    fn enable_display_switch(&self, window: &crate::viewer_window::WindowControl) -> bool {
        self.0.state.load() < 2
            && window.input_window().is_ok_and(|target| {
                target.id == self.0.window.id
                    && target.width == self.0.window.width
                    && target.height == self.0.window.height
            })
            && self.0.display_switch.set(window.clone()).is_ok()
    }
    fn first_stop_after_fence(&self) -> Option<StopReason> {
        // A reservation alone is not an input fence. The Source's actual
        // lifecycle control must be stopped before returning a terminal cause.
        if !self.0.control.is_stopped() {
            return None;
        }
        self.0
            .state
            .first_stop()
            .filter(|reason| !matches!(reason, StopReason::Local | StopReason::Closed))
    }
    /// True only after this original attachment acquired the requested native
    /// keyboard grab and completed its initialization/lifecycle barrier. Stop
    /// fences input first; native cleanup is still reported separately.
    pub fn system_shortcuts_active(&self) -> bool {
        self.0.system_shortcuts && self.status() == Status::Running
    }
    /// Bind one explicit upload selection to local Ctrl+Alt+Shift+F8/F9/F10
    /// (pause/resume/cancel). Binding is one-use, adds no input/file authority,
    /// and cannot redirect a retained capture handle to another selection.
    /// The original sender owns deadlines, receipts and actual cleanup.
    pub fn enable_file_shortcuts(&self, files: frd::native_files::SendControl) -> bool {
        self.0.state.load() < 2 && self.0.files.set(files).is_ok()
    }
    /// Physical-key mapping availability, not proof that a request was applied.
    /// None means unconfigured or native initialization has not supplied a map.
    pub fn file_shortcuts_available(&self) -> Option<bool> {
        self.0.files.get()?;
        match self.0.file_keymap.load(Ordering::Acquire) {
            1 => Some(true),
            2 => Some(false),
            _ => None,
        }
    }
}
fn state(value: u8) -> Status {
    use StopReason as R;
    match value {
        0 => Status::Opening,
        1 => Status::Running,
        2 => Status::Stopped(R::Local),
        3 => Status::Stopped(R::Closed),
        4 => Status::Stopped(R::NativeFailure),
        5 => Status::Stopped(R::FocusLost),
        6 => Status::Stopped(R::WindowChanged),
        7 => Status::Stopped(R::KeymapChanged),
        8 => Status::Stopped(R::SyntheticInput),
        9 => Status::Stopped(R::Overflow),
        10 => Status::Stopped(R::Expired),
        11 => Status::Stopped(R::Clock),
        12 => Status::Stopped(R::UnsupportedKey),
        13 => Status::Stopped(R::InitialHeld),
        15 => Status::Stopped(R::InputDevicesChanged),
        16 => Status::Stopped(R::EscapeUnavailable),
        17 => Status::Stopped(R::LocalEscape),
        18 => Status::Stopped(R::SystemShortcutsUnavailable),
        19 => Status::Stopped(R::WindowResized),
        _ => Status::Stopped(R::Queue),
    }
}
impl fmt::Debug for CaptureControl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("NativeViewerInputControl")
            .field(&self.status())
            .finish()
    }
}
/// Retain until `finish()` returns Some to prove native thread cleanup. Drop stops
/// the original viewer immediately, but cannot kill a blocked foreign X call.
#[must_use]
pub struct X11InputCapture {
    control: CaptureControl,
    task: Option<JoinHandle<()>>,
    /// The local pointer image owner started with this attachment (optional:
    /// its failure never affects input), joined with it.
    cursor: Option<crate::viewer_cursor::X11WindowCursor>,
}
impl X11InputCapture {
    /// Consume the one-use Source from `ControlledViewer::capture_input`. The
    /// window must be the application's LOCAL focused, mapped renderer window;
    /// never pass a peer-supplied XID. Native work runs only on its own thread.
    /// Failures drop/stop the original Source. No grant or viewport is created.
    pub fn start(
        display: &str,
        window: Window,
        source: Source,
        layout: Layout,
    ) -> Result<Self, Error> {
        Self::start_mode(display, window, source, layout, false)
    }
    /// Explicitly capture X11 system shortcuts for this one control attachment.
    /// This requires granted Keys, the original focused window, no held input,
    /// a usable physical emergency escape, and actual native grab acquisition.
    /// A refusal stops the original input owner; it never falls back silently.
    pub fn start_with_system_shortcuts(
        display: &str,
        window: Window,
        source: Source,
        layout: Layout,
    ) -> Result<Self, Error> {
        Self::start_mode(display, window, source, layout, true)
    }
    fn start_mode(
        display: &str,
        window: Window,
        source: Source,
        layout: Layout,
        system_shortcuts: bool,
    ) -> Result<Self, Error> {
        Self::start_bound(
            display,
            window,
            source,
            layout,
            system_shortcuts,
            #[cfg(feature = "linux-viewer-window")]
            None,
        )
    }
    fn start_bound(
        display: &str,
        window: Window,
        source: Source,
        layout: Layout,
        system_shortcuts: bool,
        #[cfg(feature = "linux-viewer-window")] display_switch: Option<
            &crate::viewer_window::WindowControl,
        >,
    ) -> Result<Self, Error> {
        if !local_display(display) {
            return Err(Error::InvalidDisplay);
        }
        if !window.valid(&layout) {
            return Err(Error::InvalidWindow);
        }
        source.clock().map_err(|_| Error::Closed)?;
        let display = CString::new(display).map_err(|_| Error::InvalidDisplay)?;
        let control = CaptureControl(Arc::new(Shared {
            control: source.control(),
            state: CaptureState::default(),
            files: OnceLock::new(),
            file_keymap: AtomicU8::new(0),
            system_shortcuts,
            #[cfg(feature = "linux-viewer-window")]
            window,
            #[cfg(feature = "linux-viewer-window")]
            display_switch: OnceLock::new(),
        }));
        // The native producer must never export a first batch before its local
        // command binding exists. This is construction-time wiring, not a live
        // toggle after attachment or a second capture owner.
        #[cfg(feature = "linux-viewer-window")]
        if let Some(window) = display_switch
            && !control.enable_display_switch(window)
        {
            control.0.stop(StopReason::NativeFailure);
            return Err(Error::InvalidWindow);
        }
        let shared = control.0.clone();
        let task = thread::Builder::new()
            .name("fr-viewer-input".into())
            .spawn(move || {
                let _fence = Fence(shared.clone());
                let mut target = NativeTarget {
                    source,
                    shared: shared.clone(),
                };
                let result = if system_shortcuts {
                    run_with_system_shortcuts(&display, window, &layout, &mut target)
                } else {
                    run(&display, window, &layout, &mut target)
                };
                shared.stop(result.err().unwrap_or(StopReason::Closed));
            })
            .map_err(|_| {
                control.0.stop(StopReason::NativeFailure);
                Error::ThreadUnavailable
            })?;
        Ok(Self {
            control,
            task: Some(task),
            cursor: None,
        })
    }
    /// Attach to an already granted original viewer from its interactive UI
    /// callback. The local renderer must have confirmed this exact Layout first.
    /// The viewer, not the UI callback, retains the native thread through every
    /// service exit. This returns only a revocation/status handle, never a Source
    /// or another input owner. Call the viewer's `input_capture_cleanup` after
    /// close until Complete to distinguish the fence from actual thread cleanup.
    pub fn attach(
        viewer: &mut frd::session_startup::ControlledViewer,
        display: &str,
        window: Window,
        layout: Layout,
    ) -> Result<CaptureControl, frd::session_startup::viewer_events::CaptureStartError<Error>> {
        Self::attach_mode(viewer, display, window, layout, false)
    }
    /// The explicit system-shortcut variant of `attach`. The existing viewer
    /// retains the same one-use source and native cleanup owner; this API adds
    /// no grant, new target, live toggle or ability to resume a stopped capture.
    pub fn attach_with_system_shortcuts(
        viewer: &mut frd::session_startup::ControlledViewer,
        display: &str,
        window: Window,
        layout: Layout,
    ) -> Result<CaptureControl, frd::session_startup::viewer_events::CaptureStartError<Error>> {
        Self::attach_mode(viewer, display, window, layout, true)
    }
    /// Install this exact window's local display-change command BEFORE the
    /// original capture thread can run. The ordinary system-shortcut choice
    /// remains explicit; this adds no control grant or automatic new selection.
    #[cfg(feature = "linux-viewer-window")]
    pub fn attach_with_display_switch(
        viewer: &mut frd::session_startup::ControlledViewer,
        display: &str,
        window: Window,
        layout: Layout,
        system_shortcuts: bool,
        display_switch: &crate::viewer_window::WindowControl,
    ) -> Result<CaptureControl, frd::session_startup::viewer_events::CaptureStartError<Error>> {
        Self::attach_bound(
            viewer,
            display,
            window,
            layout,
            system_shortcuts,
            Some(display_switch),
        )
    }
    fn attach_mode(
        viewer: &mut frd::session_startup::ControlledViewer,
        display: &str,
        window: Window,
        layout: Layout,
        system_shortcuts: bool,
    ) -> Result<CaptureControl, frd::session_startup::viewer_events::CaptureStartError<Error>> {
        Self::attach_bound(
            viewer,
            display,
            window,
            layout,
            system_shortcuts,
            #[cfg(feature = "linux-viewer-window")]
            None,
        )
    }
    fn attach_bound(
        viewer: &mut frd::session_startup::ControlledViewer,
        display: &str,
        window: Window,
        layout: Layout,
        system_shortcuts: bool,
        #[cfg(feature = "linux-viewer-window")] display_switch: Option<
            &crate::viewer_window::WindowControl,
        >,
    ) -> Result<CaptureControl, frd::session_startup::viewer_events::CaptureStartError<Error>> {
        let mut control = None;
        let mut local = None;
        let attached = viewer.capture_input_owned(&layout.clone(), |source| {
            let fence = source.control();
            let mut capture = Self::start_bound(
                display,
                window,
                source,
                layout,
                system_shortcuts,
                #[cfg(feature = "linux-viewer-window")]
                display_switch,
            )?;
            control = Some(capture.control());
            // The remote cursor's local owner (plan §11.4) shares this
            // attachment's lifetime; failing to start it only leaves the
            // platform pointer unmanaged.
            if let Ok((owner, handle)) =
                crate::viewer_cursor::X11WindowCursor::start(display, window, fence)
            {
                capture.cursor = Some(owner);
                local = Some(handle);
            }
            Ok(capture)
        });
        if let Err(error) = attached {
            // The factory may have started and fenced native capture before
            // capture_input_owned's final viewer check. Its owner stays with
            // that viewer, but preserve the native cause before this handle is
            // lost; no replacement Source or successful attachment is invented.
            if let Some(reason) = control
                .as_ref()
                .and_then(CaptureControl::first_stop_after_fence)
            {
                return Err(
                    frd::session_startup::viewer_events::CaptureStartError::Native(Error::Stopped(
                        reason,
                    )),
                );
            }
            return Err(error);
        }
        if let Some(handle) = local {
            // A refused attachment stops the handle; control is unaffected.
            let _ = viewer.attach_local_cursor(Box::new(handle));
        }
        // The factory must have completed exactly once for attachment to succeed.
        Ok(control.expect("successful native capture factory"))
    }
    pub fn control(&self) -> CaptureControl {
        self.control.clone()
    }
    /// None means cleanup is still pending, even if input has already stopped.
    /// Some is idempotent and proves this thread ended, not that host-held keys
    /// have been released. Their original host input agent owns that cleanup.
    pub fn finish(&mut self) -> Option<StopReason> {
        if self.task.as_ref().is_some_and(|task| !task.is_finished()) {
            return None;
        }
        if let Some(cursor) = &mut self.cursor {
            // Stopped with the attachment; it restores the platform pointer.
            cursor.stop();
            if !cursor.finish() {
                return None;
            }
        }
        if let Some(task) = self.task.take()
            && task.join().is_err()
        {
            #[cfg(feature = "linux-viewer-window")]
            if let Some(window) = self.control.0.display_switch.get() {
                window.cancel_resize();
            }
            self.control.0.stop(StopReason::NativeFailure);
        }
        match self.control.status() {
            Status::Stopped(reason) => Some(reason),
            _ => None,
        }
    }
}
impl fmt::Debug for X11InputCapture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.control.fmt(f)
    }
}
impl Drop for X11InputCapture {
    fn drop(&mut self) {
        self.control.retire();
        if let Some(cursor) = &self.cursor {
            cursor.stop();
        }
    }
}
struct Fence(Arc<Shared>);
impl Drop for Fence {
    fn drop(&mut self) {
        self.0.stop(StopReason::NativeFailure);
    }
}
struct Native(NonNull<c_void>);
impl Drop for Native {
    fn drop(&mut self) {
        // NativeRun has fenced the original target before dropping this owner.
        // A bounded, asynchronous barrier lets a responsive server acknowledge
        // the grab/title release; disconnect remains the terminal fallback.
        system_shortcuts::release(self);
        // SAFETY: unique C allocation, used and released on its owning thread.
        unsafe {
            fr_viewer_input_close(self.0.as_ptr());
        }
    }
}
fn local_display(display: &str) -> bool {
    let Some(number) = display.strip_prefix(':') else {
        return false;
    };
    let valid = |s: &str| !s.is_empty() && s.len() <= 5 && s.bytes().all(|b| b.is_ascii_digit());
    let mut parts = number.split('.');
    parts.next().is_some_and(valid) && parts.next().is_none_or(valid) && parts.next().is_none()
}
/// Private test seam; production has exactly the original Source implementation.
trait Target {
    fn clock(&self) -> Result<ClientInstant, StopReason>;
    fn capabilities(&self) -> Capabilities;
    fn push(&mut self, event: Event, sampled: ClientInstant) -> Result<(), StopReason>;
    fn ready(&self);
    fn file_shortcuts_enabled(&self) -> bool {
        false
    }
    fn file_shortcut_map(&self, _available: bool) {}
    fn file_command(&self, _command: file_keys::Command) {}
    fn display_switch_scope(&self) -> Option<display_switch::Scope> {
        None
    }
    fn request_display_switch(&self, _scope: display_switch::Scope, _at: ClientInstant) -> bool {
        false
    }
    fn display_switch_pending(&self) -> Option<std::time::Instant> {
        None
    }
    fn window_resized(&self, _width: u32, _height: u32) {}
    /// Fence the original input owner before any native cleanup can run.
    fn stop(&self, reason: StopReason);
}
struct NativeTarget {
    source: Source,
    shared: Arc<Shared>,
}
impl Target for NativeTarget {
    fn window_resized(&self, width: u32, height: u32) {
        // Reserve the precise native cause before the window's immediate fence
        // wakes cleanup. An earlier input stop cannot become resize intent.
        self.shared.state.stop(StopReason::WindowResized, || {
            #[cfg(feature = "linux-viewer-window")]
            if self.shared.state.first_stop() == Some(StopReason::WindowResized)
                && let Some(window) = self.shared.display_switch.get()
            {
                window.record_resize(width, height);
            }
            #[cfg(not(feature = "linux-viewer-window"))]
            let _ = (width, height);
            self.shared.control.stop();
        });
    }
    #[cfg(feature = "linux-viewer-window")]
    fn display_switch_scope(&self) -> Option<display_switch::Scope> {
        self.shared.display_switch.get()?.display_switch_scope()
    }
    #[cfg(feature = "linux-viewer-window")]
    fn request_display_switch(
        &self,
        scope: display_switch::Scope,
        captured: ClientInstant,
    ) -> bool {
        let Some(window) = self.shared.display_switch.get() else {
            return false;
        };
        // Anchor before reading the Source clock so suspension between clocks
        // can only age the gesture, never give the original press a new life.
        let observed = std::time::Instant::now();
        let Ok(now) = self.source.clock() else {
            return false;
        };
        let Some(age) = now.0.checked_sub(captured.0) else {
            return false;
        };
        let Some(at) = observed.checked_sub(Duration::from_micros(age)) else {
            return false;
        };
        window.request_display_switch(scope, at)
    }
    #[cfg(feature = "linux-viewer-window")]
    fn display_switch_pending(&self) -> Option<std::time::Instant> {
        self.shared.display_switch.get()?.display_switch_at()
    }
    fn file_shortcuts_enabled(&self) -> bool {
        self.shared.files.get().is_some()
    }
    fn file_shortcut_map(&self, available: bool) {
        self.shared
            .file_keymap
            .store(if available { 1 } else { 2 }, Ordering::Release);
    }
    fn file_command(&self, command: file_keys::Command) {
        if let Some(files) = self.shared.files.get() {
            command.apply(files);
        }
    }
    fn clock(&self) -> Result<ClientInstant, StopReason> {
        self.source.clock().map_err(|_| StopReason::Closed)
    }
    fn capabilities(&self) -> Capabilities {
        self.source.capabilities()
    }
    fn push(&mut self, event: Event, sampled: ClientInstant) -> Result<(), StopReason> {
        self.source
            .push(event, sampled)
            .map_err(|_| StopReason::Queue)
    }
    fn stop(&self, reason: StopReason) {
        self.shared.stop(reason);
    }
    fn ready(&self) {
        self.shared.state.ready();
    }
}
fn run(
    display: &CString,
    window: Window,
    layout: &Layout,
    target: &mut impl Target,
) -> Result<(), StopReason> {
    run_focused(display, window, layout, target, FOCUS_WAIT)
}
fn run_with_system_shortcuts(
    display: &CString,
    window: Window,
    layout: &Layout,
    target: &mut impl Target,
) -> Result<(), StopReason> {
    run_focused_mode(display, window, layout, target, FOCUS_WAIT, true)
}
fn run_focused(
    display: &CString,
    window: Window,
    layout: &Layout,
    target: &mut impl Target,
    focus_wait: Duration,
) -> Result<(), StopReason> {
    run_focused_mode(display, window, layout, target, focus_wait, false)
}
fn run_focused_mode(
    display: &CString,
    window: Window,
    layout: &Layout,
    target: &mut impl Target,
    focus_wait: Duration,
    system_shortcuts: bool,
) -> Result<(), StopReason> {
    let mut names = [[0; 4]; 256];
    let deadline = std::time::Instant::now() + focus_wait;
    let raw = loop {
        // The original source fences every retry: a stopped or closed viewer
        // never opens native capture.
        target.clock()?;
        let mut refusal: c_int = 0;
        // SAFETY: valid NUL string/output array/refusal slot, C retains none.
        // Opaque owner is confined to this thread. Only bounded key names
        // cross the FFI boundary.
        let raw = unsafe {
            fr_viewer_input_open(
                display.as_ptr(),
                window.id,
                window.width,
                window.height,
                &raw mut names,
                &raw mut refusal,
            )
        };
        if let Some(raw) = NonNull::new(raw) {
            break raw;
        }
        match refusal {
            OPEN_NOT_FOCUSED if std::time::Instant::now() < deadline => {
                thread::sleep(FOCUS_POLL);
            }
            OPEN_NOT_FOCUSED => return Err(StopReason::FocusLost),
            OPEN_HELD => return Err(StopReason::InitialHeld),
            OPEN_WINDOW => return Err(StopReason::WindowChanged),
            _ => return Err(StopReason::NativeFailure),
        }
    };
    let native = Native(raw);
    let guard = NativeRun { native, target };
    let result = run_events(
        &guard.native,
        window,
        layout,
        &names,
        guard.target,
        system_shortcuts,
    );
    guard
        .target
        .stop(result.err().unwrap_or(StopReason::Closed));
    result
}
/// Drop order alone is not enough: the target is a borrow and may outlive this
/// stack. Explicitly fence it BEFORE dropping the native connection, including
/// unwind. Normal exits record their precise reason before this fallback.
struct NativeRun<'a, T: Target> {
    native: Native,
    target: &'a mut T,
}
impl<T: Target> Drop for NativeRun<'_, T> {
    fn drop(&mut self) {
        self.target.stop(StopReason::NativeFailure);
    }
}
fn run_events(
    native: &Native,
    window: Window,
    layout: &Layout,
    names: &[[u8; 4]; 256],
    target: &mut impl Target,
    system_shortcuts: bool,
) -> Result<(), StopReason> {
    let mut escape = escape::LocalEscape::new(names)?;
    if system_shortcuts {
        // Acquire before the existing initialization barrier. All setup input
        // is still discarded, held gestures refused, and queued lifecycle
        // changes checked before Running or any remote event is published.
        system_shortcuts::acquire(native, target)?;
    }
    let mut shortcuts = file_keys::Shortcuts::new(names);
    let mut display_switch = display_switch::Shortcut::new(names);
    target.file_shortcut_map(shortcuts.is_some());
    let mut decoder = Decoder::new(names, target.capabilities());
    let mut timeline: Option<Timeline> = None;
    let mut sampler = held::Sampler::new(target.clock()?)?;
    loop {
        let switching = target.display_switch_pending();
        if switching.is_some_and(|at| at.elapsed() >= display_switch::CLOSE_TIMEOUT) {
            return Err(StopReason::Expired);
        }
        // No native traffic with nothing held. Held input gets bounded periodic
        // release-only sampling even when no event arrives (e.g. a lost release).
        // The first dequeued event is retained, never retimestamped as fresh.
        let first = if timeline.is_some() && switching.is_none() {
            await_event(
                native,
                target,
                &decoder,
                &sampler,
                shortcuts.as_ref(),
                display_switch.as_ref(),
            )?
        } else {
            None
        };
        let before = target.clock()?;
        let local_held = local_keys_held(target, shortcuts.as_ref(), display_switch.as_ref());
        let sample = timeline.is_some() && sampler.due_with_local(&decoder, local_held, before);
        if sample {
            sampler.begin(native, before)?;
        }
        let (events, used, barrier) = batch(native, target, window, first, before)?;
        // Local escape is negative authority only. Scan the complete bounded
        // native batch BEFORE waiting for held-state replies or exporting any
        // event. The triggering key and all its batch companions stay local.
        escape.inspect(&events[..used])?;
        // Consume both replies before the next turn, even when the snapshot is
        // unusable. Nothing here increases the original native turn deadline.
        let snapshot = if sample {
            Some(held::complete(native, target, before)?)
        } else {
            None
        };
        // Park only this native producer while the original desktop performs
        // its bounded orderly close. Keep observing escape/lifecycle failures;
        // no triggering batch, repeat or subsequent release is exported.
        if target.display_switch_pending().is_some() {
            thread::sleep(TURN);
            continue;
        }
        // Inspect the ENTIRE batch for lifecycle changes before admitting any
        // action. Native timestamps retain pre-dequeue age via a server barrier.
        if let Some(timeline) = &mut timeline {
            timeline.barrier(barrier)?;
            let Some(captured) = captured_batch(
                &events[..used],
                timeline,
                barrier,
                before,
                target,
                display_switch.as_mut(),
            )?
            else {
                thread::sleep(TURN);
                continue;
            };
            dispatch_batch(
                &events[..used],
                &captured,
                layout,
                target,
                &mut decoder,
                shortcuts.as_mut(),
            )?;
            if let Some(snapshot) = snapshot.filter(|_| held::stable(&events[..used])) {
                escape.reconcile(&snapshot.keys);
                if let Some(shortcuts) = &mut shortcuts {
                    shortcuts.reconcile(&snapshot.keys);
                }
                if let Some(shortcut) = &mut display_switch {
                    shortcut.reconcile(&snapshot.keys);
                }
                let observed = decoder.reconcile(&snapshot)?;
                // Later native events occur after this server barrier. Preserve
                // that lower bound across millisecond timestamp quantization.
                timeline.fence(barrier, before)?;
                target.push(observed, before)?;
            }
        } else {
            initialize_batch(
                &events[..used],
                layout,
                target,
                &mut decoder,
                shortcuts.as_mut(),
                display_switch.as_mut(),
            )?;
            timeline = Some(Timeline::new(barrier, before));
            target.ready();
        }
        thread::sleep(TURN);
    }
}
/// Initialization never replays input or adopts half a native gesture.
fn initialize_batch(
    events: &[Raw],
    layout: &Layout,
    target: &impl Target,
    decoder: &mut Decoder,
    mut shortcuts: Option<&mut file_keys::Shortcuts>,
    mut display_switch: Option<&mut display_switch::Shortcut>,
) -> Result<(), StopReason> {
    for event in events {
        if matches!(event.kind, 1 | 2)
            && let Some(shortcut) = &mut display_switch
        {
            let _ = shortcut.key(event.detail, event.kind == 1, None, 0);
        }
        if let Some(shortcuts) = &mut shortcuts {
            let _ = shortcuts.event(*event, false);
        }
        let _ = decoder.event(*event, layout)?;
    }
    if decoder.has_held()
        || (target.file_shortcuts_enabled()
            && shortcuts.is_some_and(|shortcuts| shortcuts.has_held()))
        || (target.display_switch_scope().is_some()
            && display_switch.is_some_and(|shortcut| shortcut.has_held()))
    {
        return Err(StopReason::InitialHeld);
    }
    Ok(())
}
fn dispatch_batch(
    events: &[Raw],
    captured: &CapturedBatch,
    layout: &Layout,
    target: &mut impl Target,
    decoder: &mut Decoder,
    mut shortcuts: Option<&mut file_keys::Shortcuts>,
) -> Result<(), StopReason> {
    for (index, event) in events.iter().enumerate() {
        if let Some(at) = captured.at[index] {
            if captured.local[index] {
                continue;
            }
            if let Some(shortcuts) = &mut shortcuts {
                match shortcuts.event(*event, target.file_shortcuts_enabled()) {
                    file_keys::Route::Local(command) => {
                        if let Some(command) = command {
                            target.file_command(command);
                        }
                        continue;
                    }
                    file_keys::Route::Remote => {}
                }
            }
            if let Some(event) = decoder.event(*event, layout)? {
                target.push(event, at)?;
            }
        }
    }
    Ok(())
}
struct CapturedBatch {
    at: [Option<ClientInstant>; MAX_NATIVE_EVENTS],
    local: [bool; MAX_NATIVE_EVENTS],
}
/// A display change creates later positive work, unlike escape. Validate all
/// native timestamps before latching intent or exporting a batch companion.
/// None parks this original producer for its owner's bounded orderly close.
fn captured_batch(
    events: &[Raw],
    timeline: &mut Timeline,
    barrier: u32,
    before: ClientInstant,
    target: &impl Target,
    mut shortcut: Option<&mut display_switch::Shortcut>,
) -> Result<Option<CapturedBatch>, StopReason> {
    let mut captured = CapturedBatch {
        at: [None; MAX_NATIVE_EVENTS],
        local: [false; MAX_NATIVE_EVENTS],
    };
    let scope = target.display_switch_scope();
    let mut request = display_switch::BatchIntent::default();
    for (index, event) in events.iter().enumerate() {
        target.clock()?;
        if (1..=5).contains(&event.kind) {
            let at = timeline.sample(event.time, barrier, before)?;
            captured.at[index] = Some(at);
            if matches!(event.kind, 1 | 2)
                && let Some(shortcut) = &mut shortcut
            {
                let route = shortcut.key(event.detail, event.kind == 1, scope, at.0);
                captured.local[index] = route.local;
                request.key(route);
            }
        }
    }
    let request = request.finish(shortcut.as_deref());
    Ok((!request.is_some_and(|gesture| {
        target.request_display_switch(gesture.scope, ClientInstant(gesture.captured))
    }))
    .then_some(captured))
}
fn batch(
    native: &Native,
    target: &impl Target,
    window: Window,
    mut first: Option<Raw>,
    before: ClientInstant,
) -> Result<([Raw; MAX_NATIVE_EVENTS], usize, u32), StopReason> {
    // SAFETY: same live native owner, at most one pending clock property.
    if unsafe { fr_viewer_input_barrier(native.0.as_ptr()) } != 1 {
        return Err(StopReason::NativeFailure);
    }
    let mut events = [Raw::default(); MAX_NATIVE_EVENTS];
    let mut used = 0;
    let barrier = loop {
        let now = target.clock()?;
        if now.0.checked_sub(before.0).ok_or(StopReason::Clock)? >= NATIVE_TURN_US {
            return Err(StopReason::Expired);
        }
        let event = if let Some(event) = first.take() {
            event
        } else {
            let mut event = Raw::default();
            // SAFETY: unique owner and a writable fixed-size output record.
            match unsafe { fr_viewer_input_next(native.0.as_ptr(), &raw mut event) } {
                0 => {
                    thread::sleep(TURN);
                    continue;
                }
                1 => event,
                _ => return Err(StopReason::NativeFailure),
            }
        };
        match event.kind {
            6 => break event.time,
            7 => return Err(StopReason::FocusLost),
            8 => return Err(StopReason::WindowChanged),
            9 if event.x != i32::try_from(window.width).unwrap()
                || event.y != i32::try_from(window.height).unwrap() =>
            {
                let width = u32::try_from(event.x).map_err(|_| StopReason::WindowChanged)?;
                let height = u32::try_from(event.y).map_err(|_| StopReason::WindowChanged)?;
                target.window_resized(width, height);
                return Err(StopReason::WindowResized);
            }
            10 => return Err(StopReason::KeymapChanged),
            11 => return Err(StopReason::SyntheticInput),
            12 => return Err(StopReason::InputDevicesChanged),
            _ => {}
        }
        if used == MAX_NATIVE_EVENTS {
            return Err(StopReason::Overflow);
        }
        events[used] = event;
        used += 1;
    };
    Ok((events, used, barrier))
}
fn await_event(
    native: &Native,
    target: &impl Target,
    decoder: &Decoder,
    sampler: &held::Sampler,
    shortcuts: Option<&file_keys::Shortcuts>,
    display_switch: Option<&display_switch::Shortcut>,
) -> Result<Option<Raw>, StopReason> {
    loop {
        target.clock()?;
        // SAFETY: original capture connection; only its own existing shortcut
        // title changes, and an unchanged value makes no X request.
        if unsafe {
            fr_viewer_input_display_switch(
                native.0.as_ptr(),
                c_int::from(target.display_switch_scope().is_some()),
            )
        } == 0
        {
            return Err(StopReason::NativeFailure);
        }
        if target.display_switch_pending().is_some() {
            return Ok(None);
        }
        let mut event = Raw::default();
        // SAFETY: unique owning thread, writable fixed-size event, no callbacks.
        match unsafe { fr_viewer_input_next(native.0.as_ptr(), &raw mut event) } {
            0 if sampler.due_with_local(
                decoder,
                local_keys_held(target, shortcuts, display_switch),
                target.clock()?,
            ) =>
            {
                return Ok(None);
            }
            0 => thread::sleep(TURN),
            1 => return Ok(Some(event)),
            _ => return Err(StopReason::NativeFailure),
        }
    }
}
fn local_keys_held(
    target: &impl Target,
    files: Option<&file_keys::Shortcuts>,
    display: Option<&display_switch::Shortcut>,
) -> bool {
    (target.file_shortcuts_enabled() && files.is_some_and(file_keys::Shortcuts::has_held))
        || (target.display_switch_scope().is_some()
            && display.is_some_and(display_switch::Shortcut::has_held))
}
struct Decoder {
    names: [[u8; 4]; 256],
    keys: [bool; 256],
    buttons: [bool; 10],
    caps: Capabilities,
}
impl Decoder {
    fn new(names: &[[u8; 4]; 256], caps: Capabilities) -> Self {
        Self {
            names: *names,
            keys: [false; 256],
            buttons: [false; 10],
            caps,
        }
    }
    fn event(&mut self, event: Raw, layout: &Layout) -> Result<Option<Event>, StopReason> {
        match event.kind {
            1 | 2 if self.caps.contains(Capability::Keys) => {
                let index =
                    usize::try_from(event.detail).map_err(|_| StopReason::UnsupportedKey)?;
                let key = self.physical_key(index)?;
                let pressed = event.kind == 1;
                let transition = match (pressed, self.keys[index]) {
                    (true, false) => KeyTransition::Press,
                    (false, true) => KeyTransition::Release,
                    (false, false) => return Err(StopReason::InitialHeld),
                    (true, true) if self.caps.contains(Capability::Repeat) => KeyTransition::Repeat,
                    (true, true) => return Ok(None),
                };
                self.keys[index] = pressed;
                Ok(Some(Event::Key { key, transition }))
            }
            3 | 4 => self.button(event, layout),
            5 if self.caps.contains(Capability::Absolute) => Ok(Some(Event::Pointer(
                layout.at(LocalPoint::pixels(event.x, event.y)),
            ))),
            _ => Ok(None),
        }
    }
    fn button(&mut self, event: Raw, layout: &Layout) -> Result<Option<Event>, StopReason> {
        let down = event.kind == 3;
        let action = match event.detail {
            // Native wheel buttons are discrete notches; the protocol uses
            // signed 16.16 distances. Never emit fractional 1/65536-line input.
            4..=7 if down && self.caps.contains(Capability::LineScroll) => {
                PositionedAction::Scroll {
                    x: match event.detail {
                        6 => -LINE,
                        7 => LINE,
                        _ => 0,
                    },
                    y: match event.detail {
                        4 => -LINE,
                        5 => LINE,
                        _ => 0,
                    },
                    unit: ScrollUnit::Lines,
                }
            }
            1..=3 | 8..=9 if self.caps.contains(Capability::Buttons) => {
                let held = &mut self.buttons[usize::try_from(event.detail).unwrap()];
                if *held == down {
                    return Err(StopReason::InitialHeld);
                }
                *held = down;
                PositionedAction::Button {
                    button: match event.detail {
                        1 => PointerButton::Primary,
                        2 => PointerButton::Middle,
                        3 => PointerButton::Secondary,
                        8 => PointerButton::Back,
                        _ => PointerButton::Forward,
                    },
                    pressed: down,
                }
            }
            _ => return Ok(None),
        };
        Ok(Some(Event::Positioned {
            location: layout.at(LocalPoint::pixels(event.x, event.y)),
            action,
        }))
    }
}

impl frd::session_startup::viewer_events::NativeCapture for X11InputCapture {
    fn stop(&self) {
        self.control.retire();
        if let Some(cursor) = &self.cursor {
            cursor.stop();
        }
    }
    fn try_reap(&mut self) -> bool {
        self.finish().is_some()
    }
}
