//! A client-owned native-pixel X11 window, never a decoder-selected input target.
//!
//! Retain the ORIGINAL viewer stop handle before starting this window or its
//! decoder. One locally created drawable is shared with the supervised decoder
//! and the existing input capture adapter. Mapping is not optical visibility,
//! observation permission, or an input grant. No implicit scaling is performed.
use fr_media::worker::{Role, presentation::X11Target};
use frd::{session_startup::StreamingViewerControl, worker::Launch};
use std::{
    ffi::{CString, c_char, c_int, c_void},
    fmt,
    future::Future,
    path::Path,
    pin::Pin,
    ptr::NonNull,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU8, AtomicU32, Ordering},
    },
    task::{Context, Poll, Waker},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const TURN_EVENTS: usize = 64;
const TURN: Duration = Duration::from_millis(10);
const MAP_TIMEOUT: Duration = Duration::from_secs(2);
unsafe extern "C" {
    fn fr_viewer_window_open(
        display: *const c_char,
        width: u32,
        height: u32,
        window: *mut u32,
    ) -> *mut c_void;
    fn fr_viewer_window_close(handle: *mut c_void);
    fn fr_viewer_window_next(handle: *mut c_void, kind: *mut u32) -> c_int;
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum StopReason {
    User = 2,
    Hidden,
    WindowLost,
    GeometryChanged,
    SessionEnded,
    NativeFailure,
    OwnerDropped,
    MappingExpired,
    EventFlood,
}
impl StopReason {
    fn from_state(value: u8) -> Option<Self> {
        match value {
            2 => Some(Self::User),
            3 => Some(Self::Hidden),
            4 => Some(Self::WindowLost),
            5 => Some(Self::GeometryChanged),
            6 => Some(Self::SessionEnded),
            7 => Some(Self::NativeFailure),
            8 => Some(Self::OwnerDropped),
            9 => Some(Self::MappingExpired),
            10 => Some(Self::EventFlood),
            _ => None,
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Opening,
    /// Server-authored map notification, not compositor visibility or consent.
    Mapped,
    Stopped(StopReason),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    InvalidDisplay,
    InvalidSize,
    SessionEnded,
    ThreadUnavailable,
    NotReady,
    NativeStopped(StopReason),
    Launch(frd::worker::Error),
}
struct Shared {
    session: StreamingViewerControl,
    state: AtomicU8,
    window: AtomicU32,
    width: u32,
    height: u32,
    waiter: Mutex<Option<Waker>>,
}
impl Shared {
    fn notify(&self) {
        let wake = self
            .waiter
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        // Never run a wake callback under the registration lock.
        if let Some(wake) = wake {
            wake.wake();
        }
    }
    fn stop(&self, reason: StopReason) {
        // No native lock, codec call, network round trip, or replacement grant.
        // Fence first; publishing Stopped never claims cleanup has completed.
        self.session.stop();
        let _ = self
            .state
            .try_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                (state < 2).then_some(reason as u8)
            });
        self.notify();
    }
    fn live(&self) -> bool {
        if self.state.load(Ordering::Acquire) >= 2 {
            return false;
        }
        if self.session.is_stopped() {
            self.stop(StopReason::SessionEnded);
            return false;
        }
        true
    }
}
/// A content-free terminal stop. It cannot change the drawable or resume input.
#[derive(Clone)]
pub struct WindowControl(Arc<Shared>);
impl WindowControl {
    pub fn stop(&self) {
        self.0.stop(StopReason::User);
    }
    pub fn status(&self) -> Status {
        self.0.live();
        match self.0.state.load(Ordering::Acquire) {
            0 => Status::Opening,
            1 => Status::Mapped,
            value => Status::Stopped(StopReason::from_state(value).expect("private state")),
        }
    }
    /// The locally created, exact-size window. A later lifecycle change can
    /// invalidate it; neither a cached target nor a numeric XID is authority.
    pub fn target(&self) -> Result<X11Target, Error> {
        if self.status() != Status::Mapped {
            return Err(Error::NotReady);
        }
        X11Target::new(
            self.0.window.load(Ordering::Acquire),
            self.0.width,
            self.0.height,
        )
        .map_err(|_| Error::NotReady)
    }
    /// Feed the SAME target to the original viewer's owned native input capture.
    /// The capture adapter still requires its actual Layout/focus/lease checks.
    pub fn input_window(&self) -> Result<crate::viewer_input::Window, Error> {
        let target = self.target()?;
        Ok(crate::viewer_input::Window {
            id: target.window(),
            width: target.width(),
            height: target.height(),
        })
    }
}
impl fmt::Debug for WindowControl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ViewerWindowControl")
            .field(&self.status())
            .finish()
    }
}
/// Keep the owner through bootstrap, streaming and media cleanup. Drop stops the
/// ORIGINAL session immediately; it does not join or kill blocked native calls.
/// Retain it and collect `finish` separately when native cleanup must be proven.
#[must_use]
pub struct ViewerWindow {
    display: String,
    control: WindowControl,
    task: Option<JoinHandle<()>>,
}
impl ViewerWindow {
    /// Only a locally selected Unix X display is accepted. Obtain `session` from
    /// `Viewer::control` or `ViewerSession::control` BEFORE decoder startup. The
    /// same handle follows subsequent observation/control promotion unchanged.
    /// Every failure fences that owner; no desktop input is enabled here.
    pub fn start(
        display: &str,
        width: u32,
        height: u32,
        session: StreamingViewerControl,
    ) -> Result<Self, Error> {
        let refused = |error| {
            session.stop();
            error
        };
        if !local_display(display) {
            return Err(refused(Error::InvalidDisplay));
        }
        X11Target::new(1, width, height).map_err(|_| refused(Error::InvalidSize))?;
        if session.is_stopped() {
            return Err(refused(Error::SessionEnded));
        }
        let native_display = CString::new(display).map_err(|_| refused(Error::InvalidDisplay))?;
        let control = WindowControl(Arc::new(Shared {
            session,
            state: AtomicU8::new(0),
            window: AtomicU32::new(0),
            width,
            height,
            waiter: Mutex::new(None),
        }));
        let shared = control.0.clone();
        let started = Instant::now();
        let task = thread::Builder::new()
            .name("fr-viewer-window".into())
            .spawn(move || {
                run(&shared, &native_display, started);
            })
            .map_err(|_| {
                control.0.stop(StopReason::NativeFailure);
                Error::ThreadUnavailable
            })?;
        Ok(Self {
            display: display.into(),
            control,
            task: Some(task),
        })
    }
    /// Wait for the actual native map event without polling timers or blocking
    /// the session runtime. At most one waiter is registered. Dropping this wait
    /// only unregisters it; the enclosing owner still decides session teardown.
    /// Mapping is not a visibility witness and does not grant input.
    pub fn ready(&mut self) -> impl Future<Output = Result<X11Target, Error>> + '_ {
        Ready { window: self }
    }
    pub fn control(&self) -> WindowControl {
        self.control.clone()
    }
    /// Use the same immutable local display and target for decoding and input.
    /// `image`/`xauthority` come from trusted local package/session configuration,
    /// never a peer, worker reply, URL, or bearer argument.
    pub fn decoder_launch(
        &self,
        image: &Path,
        xauthority: Option<&Path>,
        epoch: u128,
    ) -> Result<Launch, Error> {
        let target = self.control.target()?;
        Launch::new(image, &self.display, xauthority, Role::Present, epoch)
            .and_then(|launch| launch.present_in(target))
            .map_err(Error::Launch)
    }
    /// Nonblocking and idempotent. Some proves only this native thread ended,
    /// not remote key release, decoder cleanup, or physical pixel erasure.
    pub fn finish(&mut self) -> Option<StopReason> {
        if self.task.as_ref().is_some_and(|task| !task.is_finished()) {
            return None;
        }
        if let Some(task) = self.task.take()
            && task.join().is_err()
        {
            self.control.0.stop(StopReason::NativeFailure);
        }
        StopReason::from_state(self.control.0.state.load(Ordering::Acquire))
    }
}
impl fmt::Debug for ViewerWindow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ViewerWindow")
            .field("control", &self.control)
            .field("cleanup_collected", &self.task.is_none())
            .finish_non_exhaustive()
    }
}
impl Drop for ViewerWindow {
    fn drop(&mut self) {
        self.control.0.stop(StopReason::OwnerDropped);
    }
}
struct Native<'a> {
    handle: NonNull<c_void>,
    shared: &'a Shared,
}
impl Drop for Native<'_> {
    fn drop(&mut self) {
        // Also fence BEFORE window destruction during unwinding. An outer task
        // guard alone would run too late, after this native owner's destructor.
        self.shared.stop(StopReason::NativeFailure);
        // SAFETY: unique handle allocated and always used on this native thread.
        unsafe {
            fr_viewer_window_close(self.handle.as_ptr());
        }
    }
}
pub(crate) fn local_display(display: &str) -> bool {
    let Some(number) = display.strip_prefix(':') else {
        return false;
    };
    let mut pieces = number.split('.');
    let valid = |s: &str| !s.is_empty() && s.len() <= 5 && s.bytes().all(|b| b.is_ascii_digit());
    pieces.next().is_some_and(valid) && pieces.next().is_none_or(valid) && pieces.next().is_none()
}
fn run(shared: &Shared, display: &CString, started: Instant) {
    if !shared.live() {
        return;
    }
    let mut window = 0;
    // SAFETY: valid display and scalar pointers retained for the call only;
    // returned C resources are unique and never leave this thread.
    let Some(handle) = NonNull::new(unsafe {
        fr_viewer_window_open(
            display.as_ptr(),
            shared.width,
            shared.height,
            &raw mut window,
        )
    }) else {
        shared.stop(StopReason::NativeFailure);
        return;
    };
    let native = Native { handle, shared };
    shared.window.store(window, Ordering::Release);
    let mut mapped = false;
    loop {
        if !shared.live() {
            return;
        }
        if !mapped && started.elapsed() >= MAP_TIMEOUT {
            shared.stop(StopReason::MappingExpired);
            return;
        }
        let mut drained = false;
        for _ in 0..TURN_EVENTS {
            if !shared.live() {
                return;
            }
            let mut kind = 0;
            // SAFETY: single thread exclusively owns the handle; writable scalar.
            match unsafe { fr_viewer_window_next(native.handle.as_ptr(), &raw mut kind) } {
                0 => {
                    drained = true;
                    break;
                }
                1 => {}
                _ => {
                    shared.stop(StopReason::NativeFailure);
                    return;
                }
            }
            let reason = match kind {
                0 => None,
                1 => {
                    mapped = true;
                    None
                }
                2 => Some(StopReason::Hidden),
                3 => Some(StopReason::WindowLost),
                4 => Some(StopReason::GeometryChanged),
                5 => Some(StopReason::User),
                _ => Some(StopReason::NativeFailure),
            };
            if let Some(reason) = reason {
                shared.stop(reason);
                return;
            }
        }
        if !drained {
            shared.stop(StopReason::EventFlood);
            return;
        }
        if mapped
            && shared.live()
            && shared
                .state
                .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        {
            shared.notify();
        }
        thread::sleep(TURN);
    }
}

// Exclusive borrowing prevents concurrent ready futures from replacing each
// other's registration. Recheck after registration to close the map/stop race.
struct Ready<'a> {
    window: &'a mut ViewerWindow,
}
impl Future for Ready<'_> {
    type Output = Result<X11Target, Error>;
    fn poll(self: Pin<&mut Self>, task: &mut Context<'_>) -> Poll<Self::Output> {
        let shared = &self.window.control.0;
        if self.window.control.status() == Status::Opening {
            let mut waiter = shared
                .waiter
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !waiter
                .as_ref()
                .is_some_and(|wake| wake.will_wake(task.waker()))
            {
                *waiter = Some(task.waker().clone());
            }
        }
        match self.window.control.status() {
            Status::Opening => Poll::Pending,
            Status::Mapped => Poll::Ready(self.window.control.target()),
            Status::Stopped(reason) => Poll::Ready(Err(Error::NativeStopped(reason))),
        }
    }
}
impl Drop for Ready<'_> {
    fn drop(&mut self) {
        self.window
            .control
            .0
            .waiter
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
    }
}
