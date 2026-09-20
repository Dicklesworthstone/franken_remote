//! Bounded DAMAGE 1.x observation on the capture owner's original X connection.
//!
//! This witnesses changes to the X drawable, not optical output, GPU overlays,
//! or client visibility. The CPU-staged capture path retains periodic full pixel
//! verification. Missing DAMAGE keeps the full-readback path; no event is ever
//! interpreted as an input-authority or presentation acknowledgement.
use crate::NativeError;
use core::ffi::{c_int, c_long, c_ulong, c_void};
use core::ptr::NonNull;

#[repr(C)]
union Event {
    kind: c_int,
    padding: [c_long; 24],
}
#[link(name = "X11")]
unsafe extern "C" {
    fn XSync(display: *mut c_void, discard: c_int) -> c_int;
    fn XCheckTypedEvent(display: *mut c_void, kind: c_int, event: *mut Event) -> c_int;
}
// Public libXdamage ABI; XID and XserverRegion are unsigned long, Bool is int.
// Explicit system library, never a downloaded or peer-selected loader path.
#[link(name = "libXdamage.so.1", kind = "dylib", modifiers = "+verbatim")]
unsafe extern "C" {
    fn XDamageQueryExtension(display: *mut c_void, event: *mut c_int, error: *mut c_int) -> c_int;
    fn XDamageQueryVersion(display: *mut c_void, major: *mut c_int, minor: *mut c_int) -> c_int;
    fn XDamageCreate(display: *mut c_void, drawable: c_ulong, level: c_int) -> c_ulong;
    fn XDamageDestroy(display: *mut c_void, damage: c_ulong);
    fn XDamageSubtract(display: *mut c_void, damage: c_ulong, repair: c_ulong, parts: c_ulong);
}

/// One coalesced nonempty notification, not one event/rectangle per draw call.
/// The containing owner must destroy this BEFORE closing its X connection, and
/// forward any DAMAGE events consumed by its geometry/topology event reader.
pub(crate) struct Damage {
    display: NonNull<c_void>,
    id: c_ulong,
    event: c_int,
    dirty: bool,
}
impl Damage {
    /// # Safety
    /// `display` is the live thread-confined connection owning `drawable` and
    /// must outlive this object, including Drop. No connection is reopened.
    pub(crate) unsafe fn new(display: NonNull<c_void>, drawable: c_ulong) -> Option<Self> {
        let (mut event, mut error, mut major, mut minor) = (0, 0, 0, 0);
        // SAFETY: caller supplies the live original connection; outputs are
        // writable scalars. No returned allocation or foreign pointer escapes.
        if unsafe { XDamageQueryExtension(display.as_ptr(), &raw mut event, &raw mut error) } == 0
            || unsafe { XDamageQueryVersion(display.as_ptr(), &raw mut major, &raw mut minor) } == 0
            || major != 1
        {
            return None;
        }
        // XDamageReportNonEmpty = 3. Server-side damage accumulates until the
        // next readback boundary; do not request unbounded rectangle events.
        let id = unsafe { XDamageCreate(display.as_ptr(), drawable, 3) };
        if id == 0 {
            return None;
        }
        Some(Self {
            display,
            id,
            event,
            dirty: true,
        })
    }
    fn drain(&mut self) -> Result<(), NativeError> {
        // SAFETY: owner retains the connection, on its original thread. XSync
        // False preserves topology events. Select only our extension's event.
        unsafe { XSync(self.display.as_ptr(), 0) };
        for _ in 0..128 {
            let mut event = Event { padding: [0; 24] };
            if unsafe { XCheckTypedEvent(self.display.as_ptr(), self.event, &raw mut event) } == 0 {
                return Ok(());
            }
            self.dirty = true;
        }
        // Do not turn an event flood or incomplete drain into an idle result.
        Err(NativeError::Unavailable)
    }
    pub(crate) fn unchanged(&mut self) -> Result<bool, NativeError> {
        self.drain()?;
        Ok(!self.dirty)
    }
    pub(crate) fn before_snapshot(&mut self) -> Result<(), NativeError> {
        self.drain()?;
        self.dirty = false;
        // SAFETY: our live damage XID; None/None clears the old region without
        // allocating a returned rectangle list. Clear BEFORE XGetImage, never
        // after it: writes during readback or encoder work must remain dirty.
        unsafe { XDamageSubtract(self.display.as_ptr(), self.id, 0, 0) };
        Ok(())
    }
}
impl Drop for Damage {
    fn drop(&mut self) {
        // SAFETY: the containing owner explicitly drops this before XCloseDisplay.
        unsafe { XDamageDestroy(self.display.as_ptr(), self.id) };
    }
}
