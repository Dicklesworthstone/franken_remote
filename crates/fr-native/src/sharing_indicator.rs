//! Real X11 indicators, each backed by the ORIGINAL owner it can revoke: the
//! host's observation owner (`SharingIndicator`), or a per-lease input
//! executor's local revoke callback (`start_with` / `LocalIndicator`).
//!
//! This is a revocation-only surface, never an approval mechanism. The selected
//! desktop user, X server and window manager remain trusted. A mapped X11 window
//! is not proof of physical visibility under a compositor. No input grab, global
//! hotkey, clipboard text or peer-supplied label is installed; the only callback
//! is the local owner's own nonblocking revoke.
#[cfg(feature = "linux-session-ui")]
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
    #[cfg(feature = "linux-session-ui")]
    fn fr_indicator_open(display: *const c_char, window: *mut u32) -> *mut c_void;
    fn fr_indicator_open_control(display: *const c_char, window: *mut u32) -> *mut c_void;
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
    /// The original indicator/source lifetime stopped while awaiting mapping.
    Stopped(StopReason),
}
/// The one owner an indicator revokes, FIRST, on every stop.
enum Owner {
    #[cfg(feature = "linux-session-ui")]
    Observation(ObservationControl),
    /// A local executor's revoke. It must be idempotent, nonblocking and must
    /// not call back into the indicator.
    Local(Box<dyn Fn() + Send + Sync>),
}
impl Owner {
    fn revoke(&self) {
        match self {
            #[cfg(feature = "linux-session-ui")]
            Self::Observation(observation) => observation.revoke(),
            Self::Local(revoke) => revoke(),
        }
    }
    fn live(&self) -> bool {
        match self {
            #[cfg(feature = "linux-session-ui")]
            Self::Observation(observation) => observation.check().is_ok(),
            // The executor's lease is enforced by its broker, not here.
            Self::Local(_) => true,
        }
    }
}
#[derive(Clone, Copy)]
enum Mode {
    /// Host observation indicator (core X11 input, unchanged behavior).
    #[cfg(feature = "linux-session-ui")]
    Sharing,
    /// Remote-control indicator: only `XInput2` events from non-XTEST source
    /// devices can stop it; the remote controller injects through `XTest`.
    Control,
}
struct Shared {
    owner: Owner,
    state: AtomicU8,
    window: AtomicU32,
}
impl Shared {
    fn stop(&self, reason: StopReason) {
        // Fence the actual session/input owner FIRST, without a native/UI lock.
        // Status becoming terminal is never proof that native cleanup finished.
        self.owner.revoke();
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
        if !self.owner.live() {
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
/// Spawn the dedicated UI thread for one owner. Every failure revokes it.
fn launch(
    display: &str,
    owner: Owner,
    mode: Mode,
) -> Result<(IndicatorControl, JoinHandle<()>), Error> {
    if !local_display(display) {
        owner.revoke();
        return Err(Error::InvalidDisplay);
    }
    let Ok(display) = CString::new(display) else {
        owner.revoke();
        return Err(Error::InvalidDisplay);
    };
    let control = IndicatorControl(Arc::new(Shared {
        owner,
        state: AtomicU8::new(0),
        window: AtomicU32::new(0),
    }));
    let shared = control.0.clone();
    let started = Instant::now();
    let task = thread::Builder::new()
        .name("fr-sharing-indicator".into())
        .spawn(move || {
            let _fence = Fence(shared.clone());
            run(&shared, &display, started, mode);
        })
        .map_err(|_| {
            control.0.stop(StopReason::NativeFailure);
            Error::ThreadUnavailable
        })?;
    Ok((control, task))
}
/// Nonblocking, idempotent cleanup collection shared by both indicators.
fn collect(control: &IndicatorControl, task: &mut Option<JoinHandle<()>>) -> Option<StopReason> {
    if task.as_ref().is_some_and(|task| !task.is_finished()) {
        return None;
    }
    if let Some(task) = task.take()
        && task.join().is_err()
    {
        control.0.stop(StopReason::NativeFailure);
    }
    StopReason::from_state(control.0.state.load(Ordering::Acquire))
}

/// Explicit native thread owner. Keep it until `finish` returns Some when cleanup
/// must be proven. Drop immediately revokes but cannot kill/join a hung X server
/// call. That foreign-call limitation is NOT hidden behind an async timeout.
#[cfg(feature = "linux-session-ui")]
#[must_use]
pub struct SharingIndicator {
    control: IndicatorControl,
    task: Option<JoinHandle<()>>,
    /// The same original owner, retained for identity checks by the host.
    observation: ObservationControl,
}
#[cfg(feature = "linux-session-ui")]
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
        let (control, task) = launch(
            display,
            Owner::Observation(observation.clone()),
            Mode::Sharing,
        )?;
        Ok(Self {
            control,
            task: Some(task),
            observation,
        })
    }
    pub fn control(&self) -> IndicatorControl {
        self.control.clone()
    }
    /// Nonblocking, idempotent cleanup collection. Some proves the original UI
    /// thread ended and its own X resources were released. It says nothing about
    /// release of remote-held keys or reaping another native worker.
    pub fn finish(&mut self) -> Option<StopReason> {
        collect(&self.control, &mut self.task)
    }
}
#[cfg(feature = "linux-session-ui")]
impl fmt::Debug for SharingIndicator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The observation owner is identity, not diagnostic content.
        f.debug_struct("SharingIndicator")
            .field("control", &self.control)
            .field("cleanup_collected", &self.task.is_none())
            .finish_non_exhaustive()
    }
}
#[cfg(feature = "linux-session-ui")]
impl Drop for SharingIndicator {
    fn drop(&mut self) {
        self.control.0.stop(StopReason::OwnerDropped);
    }
}

/// Start the remote-CONTROL indicator for a local input executor that holds
/// no observation owner (`fr-input-agent`). Every stop (a real-device click on
/// STOP CONTROL or Esc/Enter/Space, closing, hiding, resizing, destruction,
/// mapping timeout, native failure or owner drop) calls `on_revoke` first.
/// Clicks and keys whose `XInput2` source is an XTEST device are ignored, so the
/// remote controller cannot operate it through the input it injects (bead
/// fr-rc-sec-approval-synthetic-input-t2r); a window-manager close cannot be
/// attributed and still stops (removal only). Without `XInput` 2 it fails to
/// open (`NativeFailure`), which the executor reports as a refused launch.
pub fn start_with(
    display: &str,
    on_revoke: impl Fn() + Send + Sync + 'static,
) -> Result<LocalIndicator, Error> {
    let (control, task) = launch(display, Owner::Local(Box::new(on_revoke)), Mode::Control)?;
    Ok(LocalIndicator {
        control,
        task: Some(task),
    })
}
/// The executor's indicator owner; shown for the whole lease. Same explicit
/// cleanup contract as `SharingIndicator`.
#[must_use]
pub struct LocalIndicator {
    control: IndicatorControl,
    task: Option<JoinHandle<()>>,
}
impl LocalIndicator {
    pub fn control(&self) -> IndicatorControl {
        self.control.clone()
    }
    pub fn finish(&mut self) -> Option<StopReason> {
        collect(&self.control, &mut self.task)
    }
}
impl fmt::Debug for LocalIndicator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalIndicator")
            .field("control", &self.control)
            .field("cleanup_collected", &self.task.is_none())
            .finish()
    }
}
impl Drop for LocalIndicator {
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
fn run(shared: &Shared, display: &CString, started: Instant, mode: Mode) {
    if !shared.live() {
        return;
    }
    let mut window = 0;
    // SAFETY: NUL-terminated display and writable scalar live throughout call.
    // C retains neither pointer. Handle is created/used/dropped on this thread.
    let handle = unsafe {
        match mode {
            #[cfg(feature = "linux-session-ui")]
            Mode::Sharing => fr_indicator_open(display.as_ptr(), &raw mut window),
            Mode::Control => fr_indicator_open_control(display.as_ptr(), &raw mut window),
        }
    };
    let Some(handle) = NonNull::new(handle) else {
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
#[cfg(feature = "linux-session-ui")]
impl frd::local_sharing::Surface for SharingIndicator {
    fn original(&self) -> &ObservationControl {
        &self.observation
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

#[cfg(feature = "linux-session-ui")]
mod mapping;
