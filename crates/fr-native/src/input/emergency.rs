//! Last-resort release for the single-owner input executor PROCESS
//! (`fr-input-agent`; bead fr-rc-sec-panic-abort-cleanup-4m7).
//!
//! A panic (also in `panic = "abort"` builds), an Xlib protocol error or a
//! fatal Xlib I/O error would otherwise end the executor with `XTest` keys or
//! buttons still held: the X server does not release them when the client
//! disappears. `install` opens a SECOND, dedicated connection before any input
//! and installs process-global Xlib error/I/O-error handlers plus a panic hook.
//! Each path releases the recorded (pessimistic) held set on that connection,
//! then EXITS; nothing continues, retries or reports success upward. If the X
//! server itself is gone nothing can be released; the broker sees EOF and
//! keeps the lease's Seat as uncertain.
//!
//! Install only in a process whose sole Xlib user is its one `X11Pointer`.
//! Handlers never return to a failed operation and never log input.
use super::{
    CodeSet, NativeHeld, PlatformError, XCloseDisplay, XOpenDisplay, XSync, XTEST,
    XTestFakeButtonEvent, XTestQueryExtension, local_display, xtest_access,
};
use core::ffi::{c_int, c_uint, c_ulong, c_void};
use std::{
    ffi::CString,
    ptr,
    sync::{
        MutexGuard, TryLockError,
        atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

/// Exit status after a panic released the recorded held set.
pub const EXIT_PANIC: i32 = 70;
/// Exit status after an Xlib protocol error.
pub const EXIT_XLIB_ERROR: i32 = 71;
/// Exit status after a fatal Xlib I/O error.
pub const EXIT_XLIB_IO: i32 = 72;
/// Bounded wait for the process-global `XTest` cache lock; see `release`.
const XTEST_WAIT: Duration = Duration::from_millis(50);

type ErrorHandler = unsafe extern "C" fn(*mut c_void, *mut c_void) -> c_int;
type IoErrorHandler = unsafe extern "C" fn(*mut c_void) -> c_int;
#[link(name = "X11")]
unsafe extern "C" {
    fn XSetErrorHandler(handler: Option<ErrorHandler>) -> Option<ErrorHandler>;
    fn XSetIOErrorHandler(handler: Option<IoErrorHandler>) -> Option<IoErrorHandler>;
}
#[link(name = "libXtst.so.6", kind = "dylib", modifiers = "+verbatim")]
unsafe extern "C" {
    fn XTestFakeKeyEvent(
        display: *mut c_void,
        code: c_uint,
        pressed: c_int,
        delay: c_ulong,
    ) -> c_int;
}

static DISPLAY: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());
static KEYS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
static BUTTONS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
static RELEASING: AtomicBool = AtomicBool::new(false);

/// Install once per process, after opening the executor's `X11Pointer` on the
/// same local display and before any input. The dedicated connection is never
/// closed; process exit closes it.
pub fn install(display: &str) -> Result<(), PlatformError> {
    if !local_display(display) {
        return Err(PlatformError::Unsupported);
    }
    crate::xlib::initialize_threads().map_err(|_| PlatformError::Unavailable)?;
    let name = CString::new(display).map_err(|_| PlatformError::Unsupported)?;
    // SAFETY: NUL-terminated name lives through the call; the returned
    // connection is owned by this module for the rest of the process.
    let connection = unsafe { XOpenDisplay(name.as_ptr()) };
    if connection.is_null() {
        return Err(PlatformError::Unavailable);
    }
    let (mut event, mut error, mut major, mut minor) = (0, 0, 0, 0);
    // Prime this connection's XTest extension cache now, not in an emergency.
    let available = {
        let _access = xtest_access();
        // SAFETY: live connection, exclusively borrowed scalar outputs, and the
        // guard excludes concurrent extension-cache mutation.
        unsafe {
            XTestQueryExtension(
                connection,
                &raw mut event,
                &raw mut error,
                &raw mut major,
                &raw mut minor,
            )
        }
    };
    if available == 0
        || DISPLAY
            .compare_exchange(
                ptr::null_mut(),
                connection,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
    {
        let _access = xtest_access();
        // SAFETY: this connection was never published; close it exactly once.
        unsafe {
            XCloseDisplay(connection);
        }
        return Err(PlatformError::Unsupported);
    }
    // SAFETY: both handlers are `extern "C"`, never unwind (they exit), and
    // retain no Rust pointers. They replace Xlib's print-and-exit defaults.
    unsafe {
        XSetErrorHandler(Some(on_error));
        XSetIOErrorHandler(Some(on_io_error));
    }
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        release();
        previous(info);
        std::process::exit(EXIT_PANIC);
    }));
    Ok(())
}

/// Publish the owner's current pessimistic held set. Lock-free, so the panic
/// hook and Xlib handlers can read it from any state of the owner.
pub fn record(held: NativeHeld) {
    for (slot, word) in KEYS.iter().zip(held.keys.words()) {
        slot.store(word, Ordering::Release);
    }
    for (slot, word) in BUTTONS.iter().zip(held.buttons.words()) {
        slot.store(word, Ordering::Release);
    }
}
/// The currently recorded set (diagnostic for the owner and its tests).
pub fn recorded() -> NativeHeld {
    let load = |slots: &[AtomicU64; 4]| {
        CodeSet::from_words(core::array::from_fn(|i| slots[i].load(Ordering::Acquire)))
    };
    NativeHeld {
        keys: load(&KEYS),
        buttons: load(&BUTTONS),
    }
}

/// One attempt per process: release every recorded code on the dedicated
/// connection and synchronize. Returns whether every release was accepted.
fn release() -> bool {
    if RELEASING.swap(true, Ordering::AcqRel) {
        return false;
    }
    let connection = DISPLAY.load(Ordering::Acquire);
    if connection.is_null() {
        return false;
    }
    let held = recorded();
    // The owner may be inside a native call holding the cache lock (possibly on
    // this very thread). Wait briefly, then proceed: in this process no other
    // thread uses Xlib, so the remaining risk is only this exiting process.
    let _access = xtest_bounded();
    let mut accepted = true;
    for code in held.keys.codes().filter(|code| *code >= 8) {
        // SAFETY: dedicated live connection; scalar release-only events with
        // zero server delay. No Rust pointer is retained.
        accepted &= unsafe { XTestFakeKeyEvent(connection, u32::from(code), 0, 0) } != 0;
    }
    for code in held.buttons.codes().filter(|code| *code != 0) {
        // SAFETY: as above, for recorded physical button codes only.
        accepted &= unsafe { XTestFakeButtonEvent(connection, u32::from(code), 0, 0) } != 0;
    }
    // SAFETY: dedicated live connection; orders server processing.
    unsafe {
        XSync(connection, 0);
    }
    accepted
}
fn xtest_bounded() -> Option<MutexGuard<'static, ()>> {
    let until = Instant::now() + XTEST_WAIT;
    loop {
        match XTEST.try_lock() {
            Ok(guard) => return Some(guard),
            Err(TryLockError::Poisoned(poisoned)) => return Some(poisoned.into_inner()),
            Err(TryLockError::WouldBlock) if Instant::now() < until => {
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(TryLockError::WouldBlock) => return None,
        }
    }
}
unsafe extern "C" fn on_error(_: *mut c_void, _: *mut c_void) -> c_int {
    // Errors raised by the release itself are ignored; the release proceeds.
    if RELEASING.load(Ordering::Acquire) {
        return 0;
    }
    release();
    std::process::exit(EXIT_XLIB_ERROR)
}
unsafe extern "C" fn on_io_error(_: *mut c_void) -> c_int {
    if !RELEASING.load(Ordering::Acquire) {
        release();
    }
    std::process::exit(EXIT_XLIB_IO)
}
