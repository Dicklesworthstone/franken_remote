//! A client-owned fixed-size X11 window, never a decoder-selected input target.
//!
//! Retain the ORIGINAL viewer stop handle before starting this window or its
//! decoder. One locally created drawable is shared with the supervised decoder
//! and the existing input capture adapter. Mapping is not optical visibility,
//! observation permission, or an input grant. The local clipboard-space policy
//! may choose a smaller initial target; later geometry changes remain terminal.
mod choice;
pub(crate) mod file_drop;
pub(crate) mod file_send;
mod focus;
pub use file_send::{
    BatchStatus as DroppedBatchStatus, Control as FileDropControl, Error as FileDropError,
};
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
        Arc, Mutex, OnceLock,
        atomic::{AtomicU8, Ordering},
    },
    task::{Context, Poll, Waker},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const TURN_EVENTS: usize = 64;
const TURN: Duration = Duration::from_millis(10);
const MAP_TIMEOUT: Duration = Duration::from_secs(2);
// Local fallback for a parked orderly-close owner, not a renewed transport or
// authority budget. Covers a network turn plus the existing 250ms exchange.
const CLOSE_TIMEOUT: Duration = Duration::from_millis(500);
const CLOSE_DISARMED: u8 = 0;
const CLOSE_ARMED: u8 = 1;
const CLOSE_REQUESTED: u8 = 2;
unsafe extern "C" {
    fn fr_viewer_window_open(
        display: *const c_char,
        width: u32,
        height: u32,
        window: *mut u32,
    ) -> *mut c_void;
    fn fr_viewer_window_open_with_controls(
        display: *const c_char,
        width: u32,
        height: u32,
        window: *mut u32,
        actual_width: *mut u32,
        actual_height: *mut u32,
    ) -> *mut c_void;
    fn fr_viewer_window_close(handle: *mut c_void);
    fn fr_viewer_window_next(handle: *mut c_void, kind: *mut u32) -> c_int;
    fn fr_viewer_window_control_offer(handle: *mut c_void, enabled: c_int) -> c_int;
    fn fr_viewer_window_control_released(handle: *mut c_void) -> c_int;
    fn fr_viewer_window_drop_offer(handle: *mut c_void, enabled: c_int) -> c_int;
    fn fr_viewer_window_drop_cancel(handle: *mut c_void);
    fn fr_viewer_window_drop_age(handle: *mut c_void) -> u32;
    fn fr_viewer_window_drop_read(handle: *mut c_void, out: *mut u8, capacity: u32) -> u32;
    fn fr_viewer_window_drop_complete(handle: *mut c_void, accepted: c_int);
    fn fr_viewer_window_control_age(handle: *mut c_void) -> u32;
    fn fr_viewer_window_control_reset(handle: *mut c_void);
    fn fr_viewer_window_focus_begin(handle: *mut c_void) -> c_int;
    fn fr_viewer_window_focus_poll(handle: *mut c_void) -> c_int;
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
    CloseExpired,
    FocusLost,
    FocusUnavailable,
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
            11 => Some(Self::CloseExpired),
            12 => Some(Self::FocusLost),
            13 => Some(Self::FocusUnavailable),
            _ => None,
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Opening,
    /// Server-authored map notification, not compositor visibility or consent.
    Mapped,
    /// Local WM close intent; no new target may be borrowed. The observation
    /// owner must retire media and close within the original bounded fallback.
    CloseRequested,
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
    orderly_close: AtomicU8,
    control_choice: choice::Choice,
    automation_focus: focus::Gate,
    file_drops: file_drop::Inbox,
    target: OnceLock<X11Target>,
    width: u32,
    height: u32,
    clipboard_space: bool,
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
        self.automation_focus.stop();
        self.file_drops.close();
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
/// Content-free local window controls. An explicit gesture is only user intent;
/// this handle cannot change the drawable, grant a lease or resume old input.
#[derive(Clone)]
pub struct WindowControl(Arc<Shared>);
impl WindowControl {
    pub fn stop(&self) {
        self.0.stop(StopReason::User);
    }
    pub(crate) fn file_drop_inbox(&self) -> file_drop::Inbox {
        self.0.file_drops.clone()
    }
    /// Whether this original window published a window-manager close request.
    /// The latch survives session cancellation and native retirement. Reading
    /// it does not poll liveness, change terminal status, or grant authority.
    pub fn close_requested(&self) -> bool {
        self.0.orderly_close.load(Ordering::Acquire) == CLOSE_REQUESTED
    }
    pub fn status(&self) -> Status {
        self.0.live();
        match self.0.state.load(Ordering::Acquire) {
            0 => Status::Opening,
            1 if self.0.orderly_close.load(Ordering::Acquire) == CLOSE_REQUESTED => {
                Status::CloseRequested
            }
            1 => Status::Mapped,
            value => Status::Stopped(StopReason::from_state(value).expect("private state")),
        }
    }
    /// Require fresh focus evidence for an explicitly automated controller on
    /// this original window. No physical event producer, keyboard grab or new
    /// input authority is created. Acquisition is bounded to thirty seconds;
    /// after the first verified focus, losing it ends this original session.
    /// Call once before requesting control and check readiness before dispatch.
    pub fn enable_automation_focus(&self) -> Result<(), Error> {
        if self.status() != Status::Mapped || !self.0.automation_focus.enable(Instant::now()) {
            return Err(Error::NotReady);
        }
        if self.status() != Status::Mapped {
            return Err(Error::NotReady);
        }
        Ok(())
    }
    /// A recent server reply named this exact local window as keyboard focus.
    /// The age includes the native round trip; unknown/stale evidence refuses.
    /// This is a best-effort local focus check, never proof of source freshness,
    /// compositor visibility, host application focus or input authorization.
    pub fn automation_focus_ready(&self) -> bool {
        self.status() == Status::Mapped
            && self.0.automation_focus.ready(Instant::now())
            && self.status() == Status::Mapped
    }
    /// Offer F8 on this fresh viewing window. It never grants input; the UI must
    /// consume a fresh gesture and use the session's ordinary control request.
    pub fn enable_control_request(&self) -> Result<(), Error> {
        if self.status() != Status::Mapped || !self.0.control_choice.enable() {
            return Err(Error::NotReady);
        }
        Ok(())
    }
    /// Missing evidence discards this gesture instead of deferring it until a
    /// future frame or focus event. Trying again needs another physical gesture.
    pub fn take_control_request(&self, ready: bool) -> bool {
        self.0
            .control_choice
            .take(ready && self.status() == Status::Mapped)
    }
    /// Only the observation-only Desktop service may arm this policy, after
    /// decoder startup. Default, control, bootstrap and direct stop stay terminal.
    pub(crate) fn enable_orderly_close(&self) -> Result<(), Error> {
        if self.status() != Status::Mapped {
            return Err(Error::NotReady);
        }
        self.0
            .orderly_close
            .compare_exchange(
                CLOSE_DISARMED,
                CLOSE_ARMED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map_err(|_| Error::NotReady)?;
        Ok(())
    }
    /// The locally created, exact-size window. A later lifecycle change can
    /// invalidate it; neither a cached target nor a numeric XID is authority.
    pub fn target(&self) -> Result<X11Target, Error> {
        if self.status() != Status::Mapped {
            return Err(Error::NotReady);
        }
        self.0.target.get().copied().ok_or(Error::NotReady)
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
        Self::start_policy(display, width, height, session, false)
    }
    /// Reserve local space for the separate clipboard off surface BEFORE the
    /// original target is created. Dimensions can only shrink from this upper
    /// bound. Use `fitted_decoder_launch` and the matching validated input
    /// viewport even if a particular screen did not need a smaller target.
    /// Window-manager placement is still checked by the independent panel;
    /// this policy never permits it to cover remote pixels or remain obscured.
    pub fn start_with_clipboard_space(
        display: &str,
        width: u32,
        height: u32,
        session: StreamingViewerControl,
    ) -> Result<Self, Error> {
        Self::start_policy(display, width, height, session, true)
    }
    fn start_policy(
        display: &str,
        width: u32,
        height: u32,
        session: StreamingViewerControl,
        clipboard_space: bool,
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
            orderly_close: AtomicU8::new(CLOSE_DISARMED),
            control_choice: choice::Choice::default(),
            automation_focus: focus::Gate::default(),
            file_drops: file_drop::Inbox::default(),
            target: OnceLock::new(),
            width,
            height,
            clipboard_space,
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
    /// Explicit local downscale into this immutable window. This retains the
    /// same native target and lifetime as `decoder_launch`; it never resizes the
    /// remote desktop or treats a changed target as renewed input authority.
    pub fn fitted_decoder_launch(
        &self,
        image: &Path,
        xauthority: Option<&Path>,
        epoch: u128,
    ) -> Result<Launch, Error> {
        let target = self.control.target()?;
        Launch::new(image, &self.display, xauthority, Role::Present, epoch)
            .and_then(|launch| launch.present_fitted_in(target))
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
fn open_native<'a>(shared: &'a Shared, display: &CString) -> Option<Native<'a>> {
    if !shared.live() {
        return None;
    }
    let mut window = 0;
    let (mut width, mut height) = (shared.width, shared.height);
    // SAFETY: valid display and scalar pointers retained for the call only;
    // returned C resources are unique and never leave this thread.
    let Some(handle) = NonNull::new(unsafe {
        if shared.clipboard_space {
            fr_viewer_window_open_with_controls(
                display.as_ptr(),
                shared.width,
                shared.height,
                &raw mut window,
                &raw mut width,
                &raw mut height,
            )
        } else {
            fr_viewer_window_open(
                display.as_ptr(),
                shared.width,
                shared.height,
                &raw mut window,
            )
        }
    }) else {
        shared.stop(StopReason::NativeFailure);
        return None;
    };
    let native = Native { handle, shared };
    let Ok(target) = X11Target::new(window, width, height) else {
        shared.stop(StopReason::NativeFailure);
        return None;
    };
    if width > shared.width
        || height > shared.height
        || (!shared.clipboard_space && (width != shared.width || height != shared.height))
        || shared.target.set(target).is_err()
    {
        shared.stop(StopReason::NativeFailure);
        return None;
    }
    // Publish one target before Mapped. Decoder, input and privacy UI borrow
    // these exact dimensions; neither reconnect nor a later WM event replaces it.
    Some(native)
}
/// Local UI only. No intent mutex is held across foreign XCB calls.
#[derive(Default)]
struct ControlGesture {
    armed: bool,
    barrier_at: Option<Instant>,
}
impl ControlGesture {
    fn offer(&mut self, native: &Native<'_>) -> Result<(), ()> {
        if !self.armed && native.shared.control_choice.offered() {
            // SAFETY: the sole window thread owns this handle and its event mask.
            match unsafe { fr_viewer_window_control_offer(native.handle.as_ptr(), 1) } {
                1 => self.armed = true,
                0 => native.shared.control_choice.unavailable(),
                _ => return Err(()),
            }
        }
        Ok(())
    }
    fn invalidate(&mut self, native: &Native<'_>, remap: bool) -> Result<(), ()> {
        self.barrier_at = None;
        native.shared.control_choice.invalidate();
        if remap && self.armed {
            // SAFETY: discard this shortcut mapping, never change the session.
            if unsafe { fr_viewer_window_control_offer(native.handle.as_ptr(), 0) } != 1 {
                return Err(());
            }
            self.armed = false;
        }
        Ok(())
    }
    fn finish(&mut self, native: &Native<'_>, chosen: bool, fresh: bool) -> Result<(), ()> {
        if !fresh {
            // SAFETY: a parked batch cannot arm a future release from an old press.
            unsafe { fr_viewer_window_control_reset(native.handle.as_ptr()) };
            self.barrier_at = None;
        }
        if !chosen || !self.armed || !fresh {
            return Ok(());
        }
        let before = self.barrier_at.take().ok_or(())?;
        // SAFETY: fixed scalars from the original owner after the whole batch.
        let age = unsafe { fr_viewer_window_control_age(native.handle.as_ptr()) };
        let Some(captured) = before.checked_sub(Duration::from_millis(u64::from(age))) else {
            return Ok(());
        };
        if captured.elapsed() >= choice::MAX_AGE {
            return Ok(());
        }
        // Native age comes from the post-release server barrier, never dequeue.
        // SAFETY: current focus and real held keys are checked independently.
        match unsafe { fr_viewer_window_control_released(native.handle.as_ptr()) } {
            0 => return Ok(()),
            1 => {}
            _ => return Err(()),
        }
        // The server acknowledges disabling this shortcut before the runtime can
        // request a new lease. The input owner inherits no event subscription.
        // SAFETY: same exclusive native owner and handle.
        if unsafe { fr_viewer_window_control_offer(native.handle.as_ptr(), 0) } != 1 {
            return Err(());
        }
        self.armed = false;
        if native.shared.live() {
            native.shared.control_choice.publish(captured);
        }
        Ok(())
    }
}
/// Local drag intent stays separate from the existing input-capture owner.
/// A server timestamp barrier fixes the original age; neither a late selection
/// reply nor polling the mailbox gives a drop another lifetime.
#[derive(Default)]
struct FileGesture {
    offered: bool,
    clock_at: Option<Instant>,
    captured: Option<Instant>,
    data: bool,
    buffer: Option<Box<[u8]>>,
}
impl FileGesture {
    fn cancel(&mut self, native: &Native<'_>) {
        // SAFETY: the original native window thread exclusively owns the handle.
        unsafe { fr_viewer_window_drop_cancel(native.handle.as_ptr()) };
        self.clock_at = None;
        self.captured = None;
        self.data = false;
    }
    fn tick(&mut self, native: &Native<'_>, fresh: bool) {
        if !fresh
            || self
                .clock_at
                .is_some_and(|at| at.elapsed() >= file_drop::MAX_AGE)
        {
            self.cancel(native);
        }
        let ready = fresh && native.shared.file_drops.ready() && native.shared.live();
        if ready != self.offered {
            // SAFETY: atom/property setup remains on this sole XCB owner.
            if unsafe { fr_viewer_window_drop_offer(native.handle.as_ptr(), i32::from(ready)) } == 1
            {
                self.offered = ready;
            } else {
                native.shared.file_drops.close();
                self.offered = false;
            }
        }
    }
    fn event(&mut self, native: &Native<'_>, kind: u32, before: Instant) {
        match kind {
            10 => {
                self.clock_at = Some(before);
                self.captured = None;
                self.data = false;
            }
            11 => {
                // SAFETY: a scalar from this exact drop's checked server barrier.
                let age = unsafe { fr_viewer_window_drop_age(native.handle.as_ptr()) };
                self.captured = self
                    .clock_at
                    .and_then(|at| at.checked_sub(Duration::from_millis(u64::from(age))));
            }
            12 => self.data = true,
            _ => {}
        }
    }
    fn finish(&mut self, native: &Native<'_>, fresh: bool) {
        if !self.data {
            return;
        }
        let mut accepted = false;
        if fresh
            && native.shared.live()
            && native.shared.file_drops.ready()
            && let Some(captured) = self.captured.filter(|at| at.elapsed() < file_drop::MAX_AGE)
        {
            let buffer = self
                .buffer
                .get_or_insert_with(|| vec![0; file_drop::MAX_BYTES].into_boxed_slice());
            // SAFETY: fixed-size owned byte storage; C checks the supplied cap,
            // writes only the retained property and never retains this pointer.
            let length = unsafe {
                fr_viewer_window_drop_read(
                    native.handle.as_ptr(),
                    buffer.as_mut_ptr(),
                    file_drop::MAX_BYTES as u32,
                )
            } as usize;
            if length <= buffer.len()
                && let Ok(paths) = file_drop::paths(&buffer[..length])
            {
                accepted = native.shared.file_drops.publish(paths, captured);
            }
            buffer.fill(0);
        }
        // This acknowledges only local selection admission. The upload panel
        // and original sender's receipts report preparation and host publication.
        // SAFETY: finish the same bounded native drop after the whole event batch.
        unsafe { fr_viewer_window_drop_complete(native.handle.as_ptr(), i32::from(accepted)) };
        self.clock_at = None;
        self.captured = None;
        self.data = false;
    }
}
fn lifecycle_event(
    shared: &Shared,
    mapped: &mut bool,
    close_started: &mut Option<Instant>,
    kind: u32,
) -> Option<StopReason> {
    match kind {
        0 => None,
        1 => {
            *mapped = true;
            None
        }
        2 => Some(StopReason::Hidden),
        3 => Some(StopReason::WindowLost),
        4 => Some(StopReason::GeometryChanged),
        5 if *mapped && shared.orderly_close.load(Ordering::Acquire) != CLOSE_DISARMED => {
            // The first WM request fixes the local deadline. Repeated
            // requests never reset it. Keep the drawable owned until the
            // original observation loop retires its decoder; all other
            // negative native events and direct stop still fence NOW.
            if close_started.is_none() {
                *close_started = Some(Instant::now());
                shared
                    .orderly_close
                    .store(CLOSE_REQUESTED, Ordering::Release);
                shared.notify();
            }
            None
        }
        5 => {
            // Publish native WM intent before stop cancels the session:
            // another thread may observe cancellation before User.
            // This unarmed path still stops immediately, with no grace.
            shared
                .orderly_close
                .store(CLOSE_REQUESTED, Ordering::Release);
            Some(StopReason::User)
        }
        _ => Some(StopReason::NativeFailure),
    }
}
/// Publish neither gesture until the complete native batch passes lifecycle
/// and flood checks. Every refusal fences the same original window owner.
fn drain_events(
    native: &Native<'_>,
    gesture: &mut ControlGesture,
    files: &mut FileGesture,
    mapped: &mut bool,
    close_started: &mut Option<Instant>,
    turn: Instant,
) -> Option<(bool, bool)> {
    let shared = native.shared;
    let mut chosen = false;
    let mut invalidated = false;
    for _ in 0..TURN_EVENTS {
        if !shared.live() {
            return None;
        }
        let mut kind = 0;
        let before = Instant::now();
        // SAFETY: single thread exclusively owns the handle; writable scalar.
        match unsafe { fr_viewer_window_next(native.handle.as_ptr(), &raw mut kind) } {
            0 => return Some((chosen, invalidated)),
            1 => {}
            _ => {
                shared.stop(StopReason::NativeFailure);
                return None;
            }
        }
        let reason = match kind {
            6 => {
                chosen = true;
                None
            }
            7 | 8 => {
                chosen = false;
                invalidated = true;
                gesture
                    .invalidate(native, kind == 8)
                    .err()
                    .map(|()| StopReason::NativeFailure)
            }
            9 => {
                gesture.barrier_at = Some(turn);
                None
            }
            10..=12 => {
                files.event(native, kind, before);
                None
            }
            13 => {
                chosen = false;
                invalidated = true;
                gesture
                    .invalidate(native, false)
                    .err()
                    .map(|()| StopReason::NativeFailure)
                    .or_else(|| shared.automation_focus.lost().err())
            }
            _ => lifecycle_event(shared, mapped, close_started, kind),
        };
        if let Some(reason) = reason {
            shared.stop(reason);
            return None;
        }
    }
    shared.stop(StopReason::EventFlood);
    None
}
fn run(shared: &Shared, display: &CString, started: Instant) {
    let Some(native) = open_native(shared, display) else {
        return;
    };
    let mut mapped = false;
    let mut close_started: Option<Instant> = None;
    let mut gesture = ControlGesture::default();
    let mut files = FileGesture::default();
    let mut focus = focus::Probe::default();
    let mut previous_turn = Instant::now();
    loop {
        if !shared.live() {
            return;
        }
        if close_started.is_some_and(|at| at.elapsed() >= CLOSE_TIMEOUT) {
            shared.stop(StopReason::CloseExpired);
            return;
        }
        if !mapped && started.elapsed() >= MAP_TIMEOUT {
            shared.stop(StopReason::MappingExpired);
            return;
        }
        let turn = Instant::now();
        let fresh = turn.duration_since(previous_turn) < choice::MAX_AGE;
        if gesture.offer(&native).is_err() {
            shared.stop(StopReason::NativeFailure);
            return;
        }
        files.tick(&native, fresh);
        let Some((chosen, invalidated)) = drain_events(
            &native,
            &mut gesture,
            &mut files,
            &mut mapped,
            &mut close_started,
            turn,
        ) else {
            return;
        };
        if gesture
            .finish(&native, chosen, fresh && !invalidated)
            .is_err()
        {
            shared.stop(StopReason::NativeFailure);
            return;
        }
        files.finish(&native, fresh && turn.elapsed() < choice::MAX_AGE);
        if let Err(reason) = focus.tick(&native) {
            shared.stop(reason);
            return;
        }
        previous_turn = turn;
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
            Status::CloseRequested => Poll::Ready(Err(Error::NotReady)),
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

#[cfg(test)]
mod tests;
