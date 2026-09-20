//! Idle drawable repair. This reports native submissions, never new source
//! observations, decode completions, or independently observed visibility.
use super::{NativeError, X11Surface, status};
use core::ffi::{c_int, c_void};

unsafe extern "C" {
    fn fr_x11_maintain_presentation(
        surface: *mut c_void,
        repainted: *mut c_int,
        retained_bytes: *mut usize,
    ) -> c_int;
}

/// Content-free accounting for a single native maintenance turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PresentationMaintenance {
    /// An exposed drawable was resubmitted from the last presented image.
    /// This does not assert that the image became visible or became fresh.
    pub repainted: bool,
    /// Owned pixel bytes: zero before first presentation, otherwise exactly
    /// width * height * 4. One fixed-size `XImage` header is additional metadata.
    pub retained_bytes: usize,
}

impl X11Surface {
    /// Coalesce exposures and repair from the last submitted picture without
    /// decoding, capturing, changing frame identity, or granting input authority.
    /// At most one tightly packed picture is retained. A lifecycle change (even
    /// resize-away-and-back), or more than 128 events in a turn, retires the owner
    /// and frees that picture. Before first presentation this submits nothing.
    ///
    /// This is Xlib work: run only in the supervised media worker, never on an
    /// authority thread. It does not make blocking foreign calls cancellable.
    pub fn maintain_presentation(&mut self) -> Result<PresentationMaintenance, NativeError> {
        let mut repainted = 0;
        let mut retained_bytes = 0;
        // SAFETY: the live thread-confined owner and both scalar outputs are
        // borrowed only for this call. Retained image ownership remains in C.
        status(unsafe {
            fr_x11_maintain_presentation(
                self.raw.as_ptr(),
                &raw mut repainted,
                &raw mut retained_bytes,
            )
        })?;
        Ok(PresentationMaintenance {
            repainted: repainted != 0,
            retained_bytes,
        })
    }
}
