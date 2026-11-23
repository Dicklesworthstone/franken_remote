//! Process-wide Xlib thread initialization, before any library display opens.
//! Applications mixing other Xlib/toolkit users must call this at process start,
//! before THEIR first Xlib call as well. Connections remain thread-confined.
use std::{
    ffi::{c_char, c_int},
    sync::OnceLock,
};
#[link(name = "X11")]
unsafe extern "C" {
    fn XInitThreads() -> c_int;
    fn XSetLocaleModifiers(modifiers: *const c_char) -> *mut c_char;
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InitializationFailed;
/// Initialize once before Xlib use, as required by XInitThreads(3). Never call
/// `XFreeThreads` while any native connection or another toolkit remains alive.
pub fn initialize_threads() -> Result<(), InitializationFailed> {
    static READY: OnceLock<bool> = OnceLock::new();
    // SAFETY: serializes the first Xlib call made through either library
    // adapter; the process-start contract covers external Xlib users. Xlib
    // owns its global thread support; no Rust pointers cross this boundary.
    if *READY.get_or_init(|| unsafe { XInitThreads() != 0 }) {
        Ok(())
    } else {
        Err(InitializationFailed)
    }
}

/// Select the configured local input method exactly once, before any of this
/// crate's composer input methods or font sets are opened. Empty modifiers ask
/// Xlib to honor XMODIFIERS; an unavailable configured method must not silently
/// become a different method. The process locale itself is never changed.
///
/// As with `initialize_threads`, an application mixing external toolkit/XIM
/// callers must initialize this at process start before their locale-dependent
/// Xlib calls. All crate-owned composer callers share this initialization.
pub fn initialize_input_method() -> Result<(), InitializationFailed> {
    static READY: OnceLock<bool> = OnceLock::new();
    initialize_threads()?;
    // SAFETY: only one mutation of Xlib's locale-modifier storage, before any
    // crate-owned XOpenIM/XCreateFontSet. The returned Xlib pointer is inspected
    // for null only and is never borrowed across another native call.
    if *READY.get_or_init(|| unsafe { !XSetLocaleModifiers(c"".as_ptr()).is_null() }) {
        Ok(())
    } else {
        Err(InitializationFailed)
    }
}
