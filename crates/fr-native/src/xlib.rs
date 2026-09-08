//! Process-wide Xlib thread initialization, before any library display opens.
//! Applications mixing other Xlib/toolkit users must call this at process start,
//! before THEIR first Xlib call as well. Connections remain thread-confined.
use std::{ffi::c_int, sync::OnceLock};
#[link(name = "X11")]
unsafe extern "C" {
    fn XInitThreads() -> c_int;
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
