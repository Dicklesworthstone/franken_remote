//! Optional, non-focus-taking local controls on the ORIGINAL desktop attempt.
//! Only content-free snapshots and supported local control requests are used.
//! Clipboard panels fence their original clipboard on close, loss or failure.
//! File/audio panels retain their settings when closed; none closes the desktop.
//! Audio preferences reuse the same bounded native panel as file progress;
//! neither a local mute nor a volume change creates observation/input authority.
pub(crate) mod clipboard;
mod model;
use fr_client::audio::AudioVolumeControl;
use frd::{
    native_files::{ReceiveControl, ReceiveStatus, SendControl, SendSnapshot},
    session_startup::StreamingViewerControl,
};
use model::Frame;
use std::{
    ffi::{CString, c_char, c_int, c_void},
    ptr::NonNull,
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
const TURN: Duration = Duration::from_millis(10);
const REFRESH: Duration = Duration::from_millis(100);
const MAX_CLICK_AGE: Duration = Duration::from_millis(250);
const MAX_EVENTS: usize = 64;

#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Direction {
    Upload = 0,
    Download = 1,
    Audio = 2,
    Clipboard = 3,
}

/// Local layout intent only. Neither reservation opens a peer resource nor
/// changes the original viewer's drawable or input mapping.
#[derive(Clone, Copy, Default)]
pub(crate) struct FilePlacement {
    pub(crate) secondary: bool,
    pub(crate) clipboard: bool,
    pub(crate) first_reserves_clipboard: bool,
}
impl FilePlacement {
    fn flags(self) -> u32 {
        u32::from(self.secondary)
            | (u32::from(self.clipboard) << 1)
            | (u32::from(self.secondary && self.first_reserves_clipboard) << 2)
    }
}

// One coherent original-owner snapshot, never a download projected into a
// fictitious sender. The larger fixed upload report has one bounded allocation
// on this optional UI thread; no snapshot survives the current rendering turn.
enum Snapshot {
    Upload(Box<SendSnapshot>),
    Download(ReceiveStatus),
    Audio { volume: f32, muted: bool },
    Clipboard(frd::native_clipboard::Status),
}

enum Transfer {
    Upload(SendControl),
    Download(ReceiveControl),
    Audio(AudioVolumeControl),
    Clipboard(frd::native_clipboard::Control),
}
impl Transfer {
    fn direction(&self) -> Direction {
        match self {
            Self::Upload(_) => Direction::Upload,
            Self::Download(_) => Direction::Download,
            Self::Audio(_) => Direction::Audio,
            Self::Clipboard(_) => Direction::Clipboard,
        }
    }
    fn snapshot(&self) -> Snapshot {
        match self {
            Self::Upload(source) => Snapshot::Upload(Box::new(source.snapshot())),
            Self::Download(source) => Snapshot::Download(source.status()),
            Self::Audio(source) => {
                let (volume, muted) = source.snapshot();
                Snapshot::Audio { volume, muted }
            }
            Self::Clipboard(source) => Snapshot::Clipboard(source.status()),
        }
    }
    fn action(&self, action: Action) {
        match self {
            Self::Upload(source) => action.apply(source),
            Self::Download(source) => {
                if action == Action::Cancel {
                    let _accepted = source.cancel();
                }
            }
            Self::Audio(source) => {
                let mut source = source.clone();
                match action {
                    Action::Quieter => source.set_volume(source.volume() - 0.1),
                    Action::Louder => source.set_volume(source.volume() + 0.1),
                    Action::Mute => {
                        source.toggle_mute();
                    }
                    _ => {}
                }
            }
            Self::Clipboard(source) => {
                if action == Action::TurnOff {
                    source.stop();
                }
            }
        }
    }
}

#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Pause = 1,
    Resume = 2,
    Cancel = 4,
    Quieter = 8,
    Louder = 16,
    Mute = 32,
    TurnOff = 64,
}
impl Action {
    fn from_raw(value: u32) -> Option<Self> {
        match value {
            1 => Some(Self::Pause),
            2 => Some(Self::Resume),
            4 => Some(Self::Cancel),
            8 => Some(Self::Quieter),
            16 => Some(Self::Louder),
            32 => Some(Self::Mute),
            64 => Some(Self::TurnOff),
            _ => None,
        }
    }
    fn apply(self, source: &SendControl) {
        // Request acceptance is not proof of pacing, rollback or cleanup.
        let _accepted = match self {
            Self::Pause => source.pause(),
            Self::Resume => source.resume(),
            Self::Cancel => source.cancel(),
            Self::Quieter | Self::Louder | Self::Mute | Self::TurnOff => return,
        };
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    InvalidDisplay,
    InvalidParent,
    SessionEnded,
    ThreadUnavailable,
    Stopped(StopReason),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Opening,
    Running,
    Stopped(StopReason),
}
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopReason {
    Hidden = 2,
    SessionEnded,
    NativeFailure,
    EventFlood,
    OwnerStopped,
    MappingExpired,
    Unresponsive,
}
fn status(state: u8) -> Status {
    match state {
        0 => Status::Opening,
        1 => Status::Running,
        2 => Status::Stopped(StopReason::Hidden),
        3 => Status::Stopped(StopReason::SessionEnded),
        5 => Status::Stopped(StopReason::EventFlood),
        6 => Status::Stopped(StopReason::OwnerStopped),
        7 => Status::Stopped(StopReason::MappingExpired),
        8 => Status::Stopped(StopReason::Unresponsive),
        _ => Status::Stopped(StopReason::NativeFailure),
    }
}
fn stop(state: &AtomicU8, reason: StopReason) {
    let _ = state.try_update(Ordering::AcqRel, Ordering::Acquire, |old| {
        (old < 2).then_some(reason as u8)
    });
}
/// Status/hide only. Hiding a clipboard privacy panel synchronously turns off
/// its original clipboard. This never acquires input or selects a replacement.
#[derive(Clone)]
pub struct Control {
    state: Arc<AtomicU8>,
    privacy: Option<Arc<clipboard::Privacy>>,
}
impl Control {
    pub fn status(&self) -> Status {
        status(self.state.load(Ordering::Acquire))
    }
    pub fn hide(&self) {
        self.stop(StopReason::Hidden);
    }
    fn stop(&self, reason: StopReason) {
        if let Some(privacy) = &self.privacy {
            privacy.stop();
        }
        stop(&self.state, reason);
    }
}
impl std::fmt::Debug for Control {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("FileProgressPanel")
            .field(&self.status())
            .finish()
    }
}
/// Retain through `finish()`; stop/hide is not proof that foreign XCB cleanup
/// returned. A stalled X server cannot hold the session's authority thread.
#[must_use]
pub struct Panel {
    control: Control,
    task: Option<JoinHandle<()>>,
    watchdog: Option<JoinHandle<()>>,
}
impl Panel {
    /// `parent` must be THIS application's mapped local viewer window, never a
    /// peer-supplied XID. No file is opened or read here, even before a grant.
    pub fn start(
        display: &str,
        parent: crate::viewer_input::Window,
        session: StreamingViewerControl,
        source: SendControl,
    ) -> Result<Self, Error> {
        Self::start_placed(display, parent, session, source, FilePlacement::default())
    }
    pub(crate) fn start_placed(
        display: &str,
        parent: crate::viewer_input::Window,
        session: StreamingViewerControl,
        source: SendControl,
        placement: FilePlacement,
    ) -> Result<Self, Error> {
        Self::start_transfer(
            display,
            parent,
            session,
            Transfer::Upload(source),
            placement,
        )
    }
    /// Show this original receive attempt. Only file-only cancellation is
    /// supported; this never starts a disk worker or creates receiver authority.
    pub fn start_receive(
        display: &str,
        parent: crate::viewer_input::Window,
        session: StreamingViewerControl,
        source: ReceiveControl,
    ) -> Result<Self, Error> {
        Self::start_receive_placed(display, parent, session, source, FilePlacement::default())
    }
    pub(crate) fn start_receive_placed(
        display: &str,
        parent: crate::viewer_input::Window,
        session: StreamingViewerControl,
        source: ReceiveControl,
        placement: FilePlacement,
    ) -> Result<Self, Error> {
        Self::start_transfer(
            display,
            parent,
            session,
            Transfer::Download(source),
            placement,
        )
    }
    /// Local preferences for this observer's actual playback owner, in either
    /// viewing role. This never touches the OS volume, microphone or host state.
    pub fn start_audio(
        display: &str,
        parent: crate::viewer_input::Window,
        session: StreamingViewerControl,
        volume: AudioVolumeControl,
    ) -> Result<Self, Error> {
        Self::start_transfer(
            display,
            parent,
            session,
            Transfer::Audio(volume),
            FilePlacement::default(),
        )
    }
    /// Mandatory visible off control for this original clipboard. A failed
    /// constructor also fences the source; readiness must precede native reads.
    pub fn start_clipboard(
        display: &str,
        parent: crate::viewer_input::Window,
        session: StreamingViewerControl,
        source: frd::native_clipboard::Control,
    ) -> Result<Self, Error> {
        let result = Self::start_transfer(
            display,
            parent,
            session,
            Transfer::Clipboard(source.clone()),
            FilePlacement::default(),
        );
        if result.is_err() {
            source.stop();
        }
        result
    }
    fn start_transfer(
        display: &str,
        parent: crate::viewer_input::Window,
        session: StreamingViewerControl,
        source: Transfer,
        placement: FilePlacement,
    ) -> Result<Self, Error> {
        let valid =
            |s: &str| !s.is_empty() && s.len() <= 5 && s.bytes().all(|b| b.is_ascii_digit());
        let Some(number) = display.strip_prefix(':') else {
            return Err(Error::InvalidDisplay);
        };
        let mut parts = number.split('.');
        if !parts.next().is_some_and(valid)
            || !parts.next().is_none_or(valid)
            || parts.next().is_some()
        {
            return Err(Error::InvalidDisplay);
        }
        if parent.id == 0 || parent.width == 0 || parent.height == 0 {
            return Err(Error::InvalidParent);
        }
        if session.is_stopped() {
            return Err(Error::SessionEnded);
        }
        let display = CString::new(display).map_err(|_| Error::InvalidDisplay)?;
        let privacy = match &source {
            Transfer::Clipboard(source) => Some(Arc::new(clipboard::Privacy::new(source.clone()))),
            _ => None,
        };
        let control = Control {
            state: Arc::new(AtomicU8::new(0)),
            privacy: privacy.clone(),
        };
        let shared = control.clone();
        let task = thread::Builder::new()
            .name(match source.direction() {
                Direction::Upload => "fr-upload-ui".into(),
                Direction::Download => "fr-download-ui".into(),
                Direction::Audio => "fr-audio-ui".into(),
                Direction::Clipboard => "fr-clipboard-ui".into(),
            })
            .spawn(move || {
                let _fence = Fence(shared.clone());
                let reason = run(
                    &display,
                    parent,
                    &shared.state,
                    &NativeTarget {
                        session,
                        source,
                        privacy,
                        placement,
                    },
                );
                shared.stop(reason);
            })
            .map_err(|_| Error::ThreadUnavailable)?;
        // A blocked XCB connection cannot remain evidence for clipboard access.
        // Retain the native task even if its independent watchdog cannot start.
        let watchdog = if control.privacy.is_some() {
            let shared = control.clone();
            match thread::Builder::new()
                .name("fr-clipboard-ui-watch".into())
                .spawn(move || clipboard::supervise(&shared))
            {
                Ok(task) => Some(task),
                Err(_) => {
                    control.stop(StopReason::NativeFailure);
                    None
                }
            }
        } else {
            None
        };
        Ok(Self {
            control,
            task: Some(task),
            watchdog,
        })
    }
    pub fn control(&self) -> Control {
        self.control.clone()
    }
    pub fn stop(&self) {
        self.control.stop(StopReason::OwnerStopped);
    }
    pub fn finish(&mut self) -> bool {
        if self.task.as_ref().is_some_and(|t| !t.is_finished())
            || self.watchdog.as_ref().is_some_and(|t| !t.is_finished())
        {
            return false;
        }
        if let Some(task) = self.task.take() {
            let _joined = task.join();
        }
        if let Some(task) = self.watchdog.take() {
            let _joined = task.join();
        }
        true
    }
}
impl Drop for Panel {
    fn drop(&mut self) {
        self.stop();
    }
}
struct Fence(Control);
impl Drop for Fence {
    fn drop(&mut self) {
        self.0.stop(StopReason::NativeFailure);
    }
}
unsafe extern "C" {
    fn fr_upload_open(
        display: *const c_char,
        parent: u32,
        width: u32,
        height: u32,
        direction: u32,
    ) -> *mut c_void;
    fn fr_upload_open_file(
        display: *const c_char,
        parent: u32,
        width: u32,
        height: u32,
        direction: u32,
        placement: u32,
    ) -> *mut c_void;
    fn fr_upload_draw(native: *mut c_void, frame: *const Frame) -> c_int;
    fn fr_upload_validate(native: *mut c_void) -> c_int;
    fn fr_upload_mapped(native: *mut c_void) -> c_int;
    fn fr_upload_next(native: *mut c_void, kind: *mut u32, action: *mut u32) -> c_int;
    fn fr_upload_close(native: *mut c_void);
    fn fr_upload_disarm(native: *mut c_void);
}
struct Native(NonNull<c_void>);
impl Drop for Native {
    fn drop(&mut self) {
        // SAFETY: uniquely owned C allocation, released on its owning thread.
        unsafe {
            fr_upload_close(self.0.as_ptr());
        }
    }
}
// Test seam is private: production always keeps the exact original handles.
trait Target {
    fn stopped(&self) -> bool;
    fn direction(&self) -> Direction;
    fn file_placement(&self) -> FilePlacement {
        FilePlacement::default()
    }
    fn snapshot(&self) -> Snapshot;
    fn action(&self, action: Action);
    fn panel_stopped(&self) {}
    fn progress(&self) {}
}
struct NativeTarget {
    session: StreamingViewerControl,
    source: Transfer,
    privacy: Option<Arc<clipboard::Privacy>>,
    placement: FilePlacement,
}
impl Target for NativeTarget {
    fn stopped(&self) -> bool {
        self.session.is_stopped()
    }
    fn direction(&self) -> Direction {
        self.source.direction()
    }
    fn file_placement(&self) -> FilePlacement {
        self.placement
    }
    fn snapshot(&self) -> Snapshot {
        self.source.snapshot()
    }
    fn action(&self, action: Action) {
        self.source.action(action);
    }
    fn panel_stopped(&self) {
        if let Some(privacy) = &self.privacy {
            privacy.stop();
        }
    }
    fn progress(&self) {
        if let Some(privacy) = &self.privacy {
            privacy.progress();
        }
    }
}
fn run(
    display: &CString,
    parent: crate::viewer_input::Window,
    state: &AtomicU8,
    target: &impl Target,
) -> StopReason {
    let _target_fence = TargetFence(target);
    if target.stopped() {
        return StopReason::SessionEnded;
    }
    if state.load(Ordering::Acquire) >= 2 {
        return StopReason::OwnerStopped;
    }
    // SAFETY: live NUL string, scalars only; the C owner retains no Rust pointer.
    let Some(native) = NonNull::new(unsafe {
        if matches!(target.direction(), Direction::Upload | Direction::Download) {
            fr_upload_open_file(
                display.as_ptr(),
                parent.id,
                parent.width,
                parent.height,
                target.direction() as u32,
                target.file_placement().flags(),
            )
        } else {
            fr_upload_open(
                display.as_ptr(),
                parent.id,
                parent.width,
                parent.height,
                target.direction() as u32,
            )
        }
    }) else {
        return StopReason::NativeFailure;
    };
    let native = Native(native);
    // Record terminal state BEFORE native disconnect, including unwind.
    let _fence = NativeFence { state, target };
    let reason = events(&native, state, target);
    target.panel_stopped();
    stop(state, reason);
    reason
}
struct TargetFence<'a, T: Target>(&'a T);
impl<T: Target> Drop for TargetFence<'_, T> {
    fn drop(&mut self) {
        self.0.panel_stopped();
    }
}
struct NativeFence<'a, T: Target> {
    state: &'a AtomicU8,
    target: &'a T,
}
impl<T: Target> Drop for NativeFence<'_, T> {
    fn drop(&mut self) {
        self.target.panel_stopped();
        stop(self.state, StopReason::NativeFailure);
    }
}
fn mark_ready(native: &Native, state: &AtomicU8, target: &impl Target) {
    // Positive mapping/drawing evidence; clipboard also requires server-authored
    // unobscured visibility. An accepted map request alone never opens its gate.
    // SAFETY: thread-confined native owner, no retained Rust pointer.
    if unsafe { fr_upload_mapped(native.0.as_ptr()) } == 1 {
        target.progress();
        let _ = state.compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire);
    }
}
fn events(native: &Native, state: &AtomicU8, target: &impl Target) -> StopReason {
    let mut previous: Option<Frame> = None;
    let mut refreshed = Instant::now();
    let mut turn = refreshed;
    loop {
        if target.stopped() {
            return StopReason::SessionEnded;
        }
        if state.load(Ordering::Acquire) >= 2 {
            return StopReason::OwnerStopped;
        }
        let now = Instant::now();
        // A stalled native turn does not freshen queued clicks. Drain them,
        // discard intents, and redraw before accepting another gesture.
        let fresh = now.duration_since(turn) < MAX_CLICK_AGE;
        let mut actions = [None; MAX_EVENTS];
        let mut used = 0;
        let mut damage = previous.is_none();
        let mut redraw = damage || now.duration_since(refreshed) >= REFRESH;
        let mut drained = false;
        for _ in 0..MAX_EVENTS {
            let (mut kind, mut action) = (0, 0);
            // SAFETY: live thread-confined native handle and fixed output slots.
            match unsafe { fr_upload_next(native.0.as_ptr(), &raw mut kind, &raw mut action) } {
                0 => {
                    drained = true;
                    break;
                }
                1 => match kind {
                    1 => {
                        damage = true;
                        redraw = true;
                    }
                    2 => return StopReason::Hidden,
                    3 => {
                        actions[used] = Action::from_raw(action);
                        used += 1;
                    }
                    _ => {}
                },
                _ => return StopReason::NativeFailure,
            }
        }
        // Lifecycle/flood checks cover the WHOLE native batch before commands.
        if !drained {
            return StopReason::EventFlood;
        }
        // Refresh geometry even when the content-free frame did not change.
        // A WM can move a reparenting ancestor without an event on our client.
        if redraw || used != 0 {
            // SAFETY: same live, thread-confined native owner; no Rust pointers.
            match unsafe { fr_upload_validate(native.0.as_ptr()) } {
                1 => {}
                0 => return StopReason::Hidden,
                _ => return StopReason::NativeFailure,
            }
        }
        if fresh && Instant::now().duration_since(now) < MAX_CLICK_AGE {
            for action in actions[..used].iter().flatten().copied() {
                if target.stopped() {
                    return StopReason::SessionEnded;
                }
                if state.load(Ordering::Acquire) >= 2 {
                    return StopReason::OwnerStopped;
                }
                // Recheck both what was displayed and the original CURRENT
                // snapshot: a shortcut or terminal receipt can race a click.
                if previous.as_ref().is_some_and(|p| p.permits(action))
                    && Frame::snapshot(&target.snapshot()).permits(action)
                {
                    target.action(action);
                    redraw = true;
                }
            }
        } else {
            // SAFETY: same live unique owner. A stale press cannot arm a later release.
            unsafe {
                fr_upload_disarm(native.0.as_ptr());
            }
            redraw = true;
        }
        if redraw {
            if target.stopped() {
                return StopReason::SessionEnded;
            }
            let frame = Frame::snapshot(&target.snapshot());
            if damage || previous.as_ref() != Some(&frame) {
                // SAFETY: fixed C-compatible record, borrowed only for this call.
                if unsafe { fr_upload_draw(native.0.as_ptr(), &raw const frame) } != 1 {
                    return StopReason::NativeFailure;
                }
            }
            previous = Some(frame);
            refreshed = Instant::now();
            mark_ready(native, state, target);
        }
        target.progress();
        turn = now;
        thread::sleep(TURN);
    }
}

#[cfg(test)]
mod tests;
