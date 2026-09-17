//! One finite native display choice bound to the original approved catalog.
//! Uses the existing XCB shell, not a second session, UI runtime or input grant.
use fr_wire::display::{Catalog, MAX_DISPLAYS};
use frd::session_startup::StreamingViewerControl;
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

const OPENING: u8 = 0;
const MAPPED: u8 = 1;
const CHOSEN: u8 = 2; // through 9: a bounded row ordinal, not a wire alias
const CONSUMED: u8 = 10;
const STOPPED: u8 = 11;
const CLOSING: u8 = 128;
const EVENTS_PER_TURN: usize = 64;
const TURN: Duration = Duration::from_millis(10);
const MAP_TIMEOUT: Duration = Duration::from_secs(2);
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Row {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}
unsafe extern "C" {
    fn fr_viewer_picker_open(
        display: *const c_char,
        rows: *const Row,
        count: u32,
        window: *mut u32,
    ) -> *mut c_void;
    fn fr_viewer_picker_next(handle: *mut c_void, kind: *mut u32, row: *mut u32) -> c_int;
    fn fr_viewer_picker_close(handle: *mut c_void);
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    EmptyCatalog,
    InvalidDisplay,
    SessionEnded,
    NativeFailure,
    Cancelled,
    CatalogChanged,
    MappingExpired,
    EventFlood,
    AlreadyUsed,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Error {}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Opening,
    Mapped,
    Decided,
    Consumed,
    Stopped(Error),
}
struct Shared {
    original: StreamingViewerControl,
    state: AtomicU8,
    window: AtomicU32,
}
impl Shared {
    fn stop(&self, reason: Error) {
        // Only an outstanding picker can stop its original attempt. A consumed
        // handle is retired and cannot close the subsequent viewing window.
        if self
            .state
            .try_update(Ordering::AcqRel, Ordering::Acquire, |s| {
                (s < CONSUMED).then_some(CLOSING + reason as u8)
            })
            .is_ok()
        {
            self.original.stop();
            self.state.store(STOPPED + reason as u8, Ordering::Release);
        } else if self.state.load(Ordering::Acquire) >= CLOSING {
            self.original.stop();
        }
    }
    fn live(&self) -> bool {
        let state = self.state.load(Ordering::Acquire);
        if state >= CLOSING {
            self.original.stop();
            return false;
        }
        if state >= CONSUMED {
            return false;
        }
        if self.original.is_stopped() {
            self.stop(Error::SessionEnded);
            return false;
        }
        true
    }
}
/// Content-free original-attempt cancellation/status, not an input authority.
#[derive(Clone)]
pub struct Control(Arc<Shared>);
impl Control {
    pub fn cancel(&self) {
        self.0.stop(Error::Cancelled);
    }
    pub fn status(&self) -> Status {
        self.0.live();
        match self.0.state.load(Ordering::Acquire) {
            OPENING => Status::Opening,
            MAPPED => Status::Mapped,
            CHOSEN..CONSUMED => Status::Decided,
            CONSUMED => Status::Consumed,
            n if n >= CLOSING => {
                self.0.original.stop();
                Status::Stopped(reason(n - CLOSING))
            }
            n => Status::Stopped(reason(n - STOPPED)),
        }
    }
    /// Local drawable for shell integration/testing only. It can disappear
    /// immediately; a numeric ID is never authority or a reusable input target.
    pub fn window(&self) -> Option<u32> {
        (self.status() == Status::Mapped).then(|| self.0.window.load(Ordering::Acquire))
    }
}
fn reason(n: u8) -> Error {
    match n {
        0 => Error::EmptyCatalog,
        1 => Error::InvalidDisplay,
        2 => Error::SessionEnded,
        4 => Error::Cancelled,
        5 => Error::CatalogChanged,
        6 => Error::MappingExpired,
        7 => Error::EventFlood,
        8 => Error::AlreadyUsed,
        _ => Error::NativeFailure,
    }
}
/// Retain this owner through startup failure and native cleanup. No operation
/// here selects a remote screen: poll returns one alias for the containing
/// original `DisplaySelection` exchange, after native cleanup is observed.
pub struct DisplayPicker {
    catalog: Catalog,
    control: Control,
    task: Option<JoinHandle<()>>,
}
impl DisplayPicker {
    pub fn start(
        display: &str,
        catalog: Catalog,
        original: StreamingViewerControl,
    ) -> Result<Self, Error> {
        let refused = |error| {
            original.stop();
            error
        };
        if catalog.displays().is_empty() {
            return Err(refused(Error::EmptyCatalog));
        }
        if !crate::viewer_window::local_display(display) {
            return Err(refused(Error::InvalidDisplay));
        }
        if original.is_stopped() {
            return Err(Error::SessionEnded);
        }
        let display = CString::new(display).map_err(|_| refused(Error::InvalidDisplay))?;
        let mut rows = [Row::default(); MAX_DISPLAYS];
        for (row, d) in rows.iter_mut().zip(catalog.displays()) {
            *row = Row {
                x: d.x,
                y: d.y,
                width: d.pixel_width,
                height: d.pixel_height,
            };
        }
        let count =
            u32::try_from(catalog.displays().len()).map_err(|_| refused(Error::EmptyCatalog))?;
        let control = Control(Arc::new(Shared {
            original,
            state: AtomicU8::new(OPENING),
            window: AtomicU32::new(0),
        }));
        let shared = control.0.clone();
        let started = Instant::now();
        let task = thread::Builder::new()
            .name("fr-display-picker".into())
            .spawn(move || run(&shared, &display, &rows, count, started))
            .map_err(|_| {
                control.0.stop(Error::NativeFailure);
                Error::NativeFailure
            })?;
        Ok(Self {
            catalog,
            control,
            task: Some(task),
        })
    }
    pub fn control(&self) -> Control {
        self.control.clone()
    }
    /// Called between the original session's bounded network turns. A changed
    /// catalog (including revision/order/geometry) is terminal, never retargeted.
    /// Native selection is returned once, only AFTER its native owner is joined.
    pub fn poll(&mut self, catalog: &Catalog) -> Result<Option<u128>, Error> {
        if self.control.status() == Status::Consumed {
            return Err(Error::AlreadyUsed);
        }
        if catalog != &self.catalog {
            self.control.0.stop(Error::CatalogChanged);
            return Err(Error::CatalogChanged);
        }
        match self.control.status() {
            Status::Stopped(e) => Err(e),
            Status::Opening | Status::Mapped => Ok(None),
            Status::Consumed => Err(Error::AlreadyUsed),
            Status::Decided => {
                if !self.finish() {
                    return Ok(None);
                }
                if !self.control.0.live() {
                    return Err(Error::SessionEnded);
                }
                let state = self.control.0.state.load(Ordering::Acquire);
                let row = state.checked_sub(CHOSEN).ok_or(Error::NativeFailure)?;
                let d = self
                    .catalog
                    .displays()
                    .get(usize::from(row))
                    .ok_or(Error::NativeFailure)?;
                self.control
                    .0
                    .state
                    .compare_exchange(state, CONSUMED, Ordering::AcqRel, Ordering::Acquire)
                    .map_err(|_| Error::SessionEnded)?;
                // This is a local choice, not presentation evidence or input.
                Ok(Some(d.handle))
            }
        }
    }
    /// Nonblocking, idempotent native cleanup observation. Stop is separate;
    /// false retains a live/draining native thread, never permission to retry.
    pub fn finish(&mut self) -> bool {
        if self.task.as_ref().is_some_and(|task| !task.is_finished()) {
            return false;
        }
        if let Some(task) = self.task.take()
            && task.join().is_err()
        {
            self.control.0.stop(Error::NativeFailure);
        }
        true
    }
}
impl fmt::Debug for DisplayPicker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DisplayPicker")
            .field("status", &self.control.status())
            .finish_non_exhaustive()
    }
}
impl Drop for DisplayPicker {
    fn drop(&mut self) {
        self.control.cancel();
    }
}
struct Native<'a> {
    handle: NonNull<c_void>,
    shared: &'a Shared,
    handoff: bool,
}
impl Drop for Native<'_> {
    fn drop(&mut self) {
        if !self.handoff {
            self.shared.stop(Error::NativeFailure);
        }
        // SAFETY: allocated and used only on this thread; no Rust pointer retained.
        unsafe {
            fr_viewer_picker_close(self.handle.as_ptr());
        }
    }
}
fn run(
    shared: &Shared,
    display: &CString,
    rows: &[Row; MAX_DISPLAYS],
    count: u32,
    started: Instant,
) {
    if !shared.live() {
        return;
    }
    let mut window = 0;
    // SAFETY: repr(C) rows have `count <= MAX_DISPLAYS` valid elements; C copies
    // them during this call and retains neither rows nor the CString pointer.
    let Some(handle) = NonNull::new(unsafe {
        fr_viewer_picker_open(display.as_ptr(), rows.as_ptr(), count, &raw mut window)
    }) else {
        shared.stop(Error::NativeFailure);
        return;
    };
    let mut native = Native {
        handle,
        shared,
        handoff: false,
    };
    shared.window.store(window, Ordering::Release);
    let mut mapped = false;
    loop {
        if !shared.live() {
            return;
        }
        if !mapped && started.elapsed() >= MAP_TIMEOUT {
            shared.stop(Error::MappingExpired);
            return;
        }
        let mut drained = false;
        let mut choice = None;
        for _ in 0..EVENTS_PER_TURN {
            if !shared.live() {
                return;
            }
            let (mut kind, mut row) = (0, 0);
            // SAFETY: unique native handle and writable scalar outputs, no callbacks.
            match unsafe {
                fr_viewer_picker_next(native.handle.as_ptr(), &raw mut kind, &raw mut row)
            } {
                0 => {
                    drained = true;
                    break;
                }
                1 => {}
                _ => {
                    shared.stop(Error::NativeFailure);
                    return;
                }
            }
            match kind {
                0 => {}
                1 => mapped = true,
                2 => {
                    shared.stop(Error::Cancelled);
                    return;
                }
                3 if mapped && row < count => {
                    if choice.replace(row).is_some_and(|previous| previous != row) {
                        shared.stop(Error::Cancelled);
                        return;
                    }
                }
                _ => {
                    shared.stop(Error::NativeFailure);
                    return;
                }
            }
        }
        if !drained {
            shared.stop(Error::EventFlood);
            return;
        }
        if mapped && shared.live() {
            let _ =
                shared
                    .state
                    .compare_exchange(OPENING, MAPPED, Ordering::AcqRel, Ordering::Acquire);
        }
        if let Some(row) = choice {
            // Close the picker before publishing the decision. Cancellation while
            // a native close is blocked still fences the original session.
            native.handoff = true;
            drop(native);
            if shared.live() {
                let code = CHOSEN + u8::try_from(row).expect("bounded native row");
                let _ = shared.state.compare_exchange(
                    MAPPED,
                    code,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                );
            }
            return;
        }
        thread::sleep(TURN);
    }
}
