//! A real X11 sharing indicator backed by the ORIGINAL observation owner.
//!
//! This is a revocation-only surface, never an approval mechanism. The selected
//! desktop user, X server and window manager remain trusted. A mapped X11 window
//! is not proof of physical visibility under a compositor. No input grab, global
//! hotkey, arbitrary callback, clipboard text or peer-supplied label is installed.
use frd::media::ObservationControl;
use std::{
    ffi::{CString, c_char, c_int, c_void},
    fmt,
    ptr::NonNull,
    sync::{
        Arc,
        atomic::{AtomicU8, AtomicU32, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const TURN_EVENTS: usize = 32;
const TURN: Duration = Duration::from_millis(10);
const MAP_TIMEOUT: Duration = Duration::from_secs(2);

unsafe extern "C" {
    fn fr_indicator_open(display: *const c_char, window: *mut u32) -> *mut c_void;
    fn fr_indicator_close(handle: *mut c_void);
    fn fr_indicator_next(handle: *mut c_void, kind: *mut u32) -> c_int;
    fn fr_indicator_draw(handle: *mut c_void) -> c_int;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum StopReason {
    User = 2,
    Hidden,
    WindowLost,
    AuthorityEnded,
    NativeFailure,
    OwnerDropped,
    MappingExpired,
}
impl StopReason {
    fn from_state(value: u8) -> Option<Self> {
        match value {
            2 => Some(Self::User),
            3 => Some(Self::Hidden),
            4 => Some(Self::WindowLost),
            5 => Some(Self::AuthorityEnded),
            6 => Some(Self::NativeFailure),
            7 => Some(Self::OwnerDropped),
            8 => Some(Self::MappingExpired),
            _ => None,
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Opening,
    /// Server-authored `MapNotify` was received and drawing was submitted. This
    /// reports X11 mapping, not compositor or human-observation qualification.
    Mapped,
    Stopped(StopReason),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    InvalidDisplay,
    AuthorityEnded,
    ThreadUnavailable,
}
struct Shared {
    observation: ObservationControl,
    state: AtomicU8,
    window: AtomicU32,
}
impl Shared {
    fn stop(&self, reason: StopReason) {
        // Fence the actual session/input owner FIRST, without a native/UI lock.
        // Status becoming terminal is never proof that native cleanup finished.
        self.observation.revoke();
        let _ = self
            .state
            .try_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                (state < 2).then_some(reason as u8)
            });
    }
    fn live(&self) -> bool {
        if self.state.load(Ordering::Acquire) >= 2 {
            return false;
        }
        if self.observation.check().is_err() {
            self.stop(StopReason::AuthorityEnded);
            return false;
        }
        true
    }
}
/// Cloneable content-free stop handle. It never owns XCB or waits for the UI
/// thread, network progress, or native media/clipboard work. It cannot resume.
#[derive(Clone)]
pub struct IndicatorControl(Arc<Shared>);
impl IndicatorControl {
    pub fn stop(&self) {
        self.0.stop(StopReason::User);
    }
    pub fn status(&self) -> Status {
        match self.0.state.load(Ordering::Acquire) {
            0 => Status::Opening,
            1 => Status::Mapped,
            value => Status::Stopped(StopReason::from_state(value).expect("private state")),
        }
    }
    /// Local X resource ID for window-manager integration. Not a network route,
    /// session identity, authorization token, or permission to attach input.
    pub fn window(&self) -> Option<u32> {
        match self.0.window.load(Ordering::Acquire) {
            0 => None,
            id => Some(id),
        }
    }
}
impl fmt::Debug for IndicatorControl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("SharingIndicatorControl")
            .field(&self.status())
            .finish()
    }
}
/// Explicit native thread owner. Keep it until `finish` returns Some when cleanup
/// must be proven. Drop immediately revokes but cannot kill/join a hung X server
/// call. That foreign-call limitation is NOT hidden behind an async timeout.
#[must_use]
pub struct SharingIndicator {
    control: IndicatorControl,
    task: Option<JoinHandle<()>>,
}
impl SharingIndicator {
    /// Start on the LOCAL selected user's X server. No ambient DISPLAY or TCP
    /// display is accepted. Supply the original `HostSession::observation()`
    /// owner before starting capture; this function creates no authority.
    /// All failure paths revoke the supplied original owner. Native initialization
    /// occurs only on its dedicated thread, not on the broker/runtime thread.
    pub fn start(display: &str, observation: ObservationControl) -> Result<Self, Error> {
        if !local_display(display) {
            observation.revoke();
            return Err(Error::InvalidDisplay);
        }
        if observation.check().is_err() {
            observation.revoke();
            return Err(Error::AuthorityEnded);
        }
        let display = CString::new(display).map_err(|_| Error::InvalidDisplay)?;
        let control = IndicatorControl(Arc::new(Shared {
            observation,
            state: AtomicU8::new(0),
            window: AtomicU32::new(0),
        }));
        let shared = control.0.clone();
        let started = Instant::now();
        let task = thread::Builder::new()
            .name("fr-sharing-indicator".into())
            .spawn(move || {
                let _fence = Fence(shared.clone());
                run(&shared, &display, started);
            })
            .map_err(|_| {
                control.0.stop(StopReason::NativeFailure);
                Error::ThreadUnavailable
            })?;
        Ok(Self {
            control,
            task: Some(task),
        })
    }
    pub fn control(&self) -> IndicatorControl {
        self.control.clone()
    }
    /// Nonblocking, idempotent cleanup collection. Some proves the original UI
    /// thread ended and its own X resources were released. It says nothing about
    /// release of remote-held keys or reaping another native worker.
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
impl fmt::Debug for SharingIndicator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SharingIndicator")
            .field("control", &self.control)
            .field("cleanup_collected", &self.task.is_none())
            .finish()
    }
}
impl Drop for SharingIndicator {
    fn drop(&mut self) {
        self.control.0.stop(StopReason::OwnerDropped);
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
        // SAFETY: unique thread-local handle allocated by the matching C opener.
        unsafe {
            fr_indicator_close(self.0.as_ptr());
        }
    }
}
fn local_display(display: &str) -> bool {
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
    // SAFETY: NUL-terminated display and writable scalar live throughout call.
    // C retains neither pointer. Handle is created/used/dropped on this thread.
    let Some(handle) =
        NonNull::new(unsafe { fr_indicator_open(display.as_ptr(), &raw mut window) })
    else {
        shared.stop(StopReason::NativeFailure);
        return;
    };
    let native = Native(handle);
    if !shared.live() {
        return;
    }
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
        let mut draw = false;
        for _ in 0..TURN_EVENTS {
            if !shared.live() {
                return;
            }
            let mut kind = 0;
            // SAFETY: same uniquely accessed native owner and writable scalar.
            match unsafe { fr_indicator_next(native.0.as_ptr(), &raw mut kind) } {
                0 => break,
                1 => {}
                _ => {
                    shared.stop(StopReason::NativeFailure);
                    return;
                }
            }
            match kind {
                0 => {}
                1 => draw = true,
                2 => {
                    mapped = true;
                    draw = true;
                }
                3 => {
                    shared.stop(StopReason::Hidden);
                    return;
                }
                4 => {
                    shared.stop(StopReason::WindowLost);
                    return;
                }
                5 => {
                    shared.stop(StopReason::User);
                    return;
                }
                _ => {
                    shared.stop(StopReason::NativeFailure);
                    return;
                }
            }
        }
        if draw && shared.live() {
            // SAFETY: single native thread; C copies only constant UI strings.
            if unsafe { fr_indicator_draw(native.0.as_ptr()) } == 0 {
                shared.stop(StopReason::NativeFailure);
                return;
            }
            if mapped {
                // A concurrent stop can never be overwritten by map completion.
                let _ = shared
                    .state
                    .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire);
            }
        }
        thread::sleep(TURN);
    }
}

/// The host can retain this native owner throughout bootstrap/control promotion.
/// Waiting for Ready never grants observation or proves compositor visibility.
impl frd::local_sharing::Surface for SharingIndicator {
    fn original(&self) -> &ObservationControl {
        &self.control.0.observation
    }
    fn state(&self) -> frd::local_sharing::State {
        use frd::local_sharing::State;
        match self.control.status() {
            Status::Opening => State::Opening,
            Status::Mapped => State::Ready,
            Status::Stopped(_) => State::Stopped,
        }
    }
    fn stop(&self) {
        self.control.0.stop(StopReason::AuthorityEnded);
    }
    fn finish(&mut self) -> bool {
        SharingIndicator::finish(self).is_some()
    }
}
