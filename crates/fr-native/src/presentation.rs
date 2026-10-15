//! Idle drawable repair. This reports native submissions, never new source
//! observations, decode completions, or independently observed visibility.
use super::{NativeError, X11Surface, status};
use crate::cursor::{Area, CursorSnapshot, snapshot};
use core::ffi::{c_int, c_ulong, c_void};
use core::ptr::NonNull;
use std::os::fd::{AsRawFd, BorrowedFd};

unsafe extern "C" {
    fn fr_x11_present_regions(
        surface: *mut c_void,
        bytes: *const u8,
        length: usize,
        regions: *const [u32; 4],
        count: usize,
    ) -> c_int;
    fn fr_x11_damage_source(
        surface: *mut c_void,
        display: *mut *mut c_void,
        drawable: *mut c_ulong,
    ) -> c_int;

    fn fr_x11_wait_presentation_input(
        surface: *mut c_void,
        input: c_int,
        input_ready: *mut c_int,
    ) -> c_int;
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
    /// Owned pixel bytes: exactly width * height * 4 when allocated. Ordinary
    /// presenters allocate on first picture; confined decoders reserve before
    /// entering seccomp. Reservation alone never paints a frame. One fixed-size
    /// native image/transfer header is additional metadata.
    pub retained_bytes: usize,
}

impl X11Surface {
    /// Update at most two `(x, y, width, height)` rectangles of the last
    /// submitted image. The caller must preserve every pixel outside them;
    /// this is local retained-image maintenance, not a fresh decoded frame.
    /// A first full `present` is required, even for an empty region list.
    ///
    /// Bounds are checked before any copy. The original native buffer is reused
    /// on both shared-memory and socket paths, and native completion precedes
    /// reuse. An exposure redraws the complete retained image without recopying
    /// it. Empty damage still checks lifecycle/repairs exposure, but otherwise
    /// sends no image. This never raises the window or grants input authority.
    pub fn present_regions(
        &mut self,
        frame: &super::BgraFrame,
        regions: &[[u32; 4]],
    ) -> Result<(), NativeError> {
        if (frame.width(), frame.height()) != (self.width(), self.height()) {
            return Err(NativeError::GeometryChanged);
        }
        if regions.len() > 2
            || regions.iter().any(|&[x, y, width, height]| {
                width == 0
                    || height == 0
                    || x >= self.width()
                    || y >= self.height()
                    || width > self.width() - x
                    || height > self.height() - y
            })
        {
            return Err(NativeError::InvalidConfiguration);
        }
        // SAFETY: immutable packed pixels and at most two ABI-matching arrays
        // live for this synchronous call. C independently validates all bounds
        // before modifying its sole retained image, and retains no Rust pointer.
        status(unsafe {
            fr_x11_present_regions(
                self.raw.as_ptr(),
                frame.pixels().as_ptr(),
                frame.pixels().len(),
                regions.as_ptr(),
                regions.len(),
            )
        })
    }

    /// Snapshot counters from this original native owner, without capturing.
    pub fn transfer_statistics(&self) -> Result<crate::image_transfer::Statistics, NativeError> {
        let mut raw = crate::image_transfer::Raw::default();
        // SAFETY: this live surface is thread-confined; output is a fixed scalar structure.
        unsafe { crate::image_transfer::fr_x11_transfer_stats(self.raw.as_ptr(), &raw mut raw) };
        raw.decode()
    }

    /// Block until the original X connection needs maintenance or the parent's
    /// unbuffered command pipe can be read (including EOF). Parent readiness has
    /// priority over queued native events. `false` requires one bounded
    /// `maintain_presentation` turn before waiting again. This does not read,
    /// duplicate, or own `input`, and must not be paired with a buffered reader
    /// that may already hold the next command. No timer or helper thread is used.
    /// Run only in the independently supervised media process, not in its
    /// authority owner: a stopped X server can still stall foreign work.
    pub fn wait_for_presentation_input(
        &mut self,
        input: BorrowedFd<'_>,
    ) -> Result<bool, NativeError> {
        let mut ready = 0;
        // SAFETY: both the thread-confined X owner and borrowed descriptor remain
        // live for this call. Poll only borrows descriptors and one scalar output.
        status(unsafe {
            fr_x11_wait_presentation_input(self.raw.as_ptr(), input.as_raw_fd(), &raw mut ready)
        })?;
        Ok(ready != 0)
    }

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

impl X11Surface {
    /// Observe the separately captured cursor without reading or changing desktop
    /// pixels. Presentation destinations are refused, even when numerically equal
    /// to a capture window. Missing XFIXES is explicit, not fabricated video.
    pub fn capture_cursor(&mut self) -> Result<Option<CursorSnapshot>, NativeError> {
        let (mut display, mut root) = (core::ptr::null_mut(), 0);
        // SAFETY: this C accessor permits only the original root capture owner.
        super::status(unsafe {
            fr_x11_damage_source(self.raw.as_ptr(), &raw mut display, &raw mut root)
        })?;
        // SAFETY: same live capture connection; snapshot borrows only for this call.
        let result = unsafe {
            snapshot(
                NonNull::new(display).ok_or(NativeError::DisplayUnavailable)?,
                root,
                Area {
                    x: 0,
                    y: 0,
                    width: self.width(),
                    height: self.height(),
                },
                &self.limits,
            )
        };
        self.revalidate()?;
        result
    }
}
