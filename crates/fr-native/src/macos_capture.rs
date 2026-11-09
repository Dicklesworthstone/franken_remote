//! macOS host capture (fr-p1-host-macos-vif): `ScreenCaptureKit` frames go
//! straight into a `VideoToolbox` HEVC session (`macos_capture.m`), with no CPU
//! copy of the picture. Every access unit is stripped of encoder SEI,
//! admitted by the same strict `HevcGuard` as the Linux encoders, and leaves
//! as canonical four-byte-length NALs.
//!
//! Change detection is the capture system's own: a frame counts as changed
//! when `ScreenCaptureKit` delivered a new complete frame since the last
//! encode. Its idle frames (nothing on screen changed) never reach the encoder.
use crate::NativeError;
use core::ffi::{c_int, c_void};
use core::ptr::NonNull;
use fr_core::{ids::RecoveryGeneration, limits::ProtocolLimits};
use fr_media::{
    access_unit::{EncodedAccessUnit, FrameId, FrameKind},
    config::{CodecConfiguration, ColorInfo},
    hevc::{HevcGuard, framing::annex_b_to_length_prefixed, strip_encoder_sei},
    worker::UnchangedCapture,
};

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct RawDisplay {
    display_id: u32,
    x: i32,
    y: i32,
    pixel_width: u32,
    pixel_height: u32,
    point_width: u32,
    point_height: u32,
}

unsafe extern "C" {
    fn fr_cap_displays(out: *mut RawDisplay, capacity: usize, count: *mut usize) -> c_int;
    fn fr_cap_open(
        display_id: u32,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
        out: *mut *mut c_void,
    ) -> c_int;
    fn fr_cap_encode(
        handle: *mut c_void,
        force_idr: c_int,
        only_if_changed: c_int,
        timeout_ms: u32,
        buffer: *mut u8,
        capacity: usize,
        length: *mut usize,
        idr: *mut c_int,
    ) -> c_int;
    fn fr_cap_set_bitrate(handle: *mut c_void, bitrate: u32) -> c_int;
    fn fr_cap_check(handle: *mut c_void) -> c_int;
    fn fr_cap_free(handle: *mut c_void);
    fn fr_cap_display_check(expected: *const RawDisplay) -> c_int;
    fn fr_cap_bind_parent(expected: i32) -> c_int;
}

const UNCHANGED: c_int = 1;
fn status(code: c_int) -> Result<(), NativeError> {
    match code {
        0 => Ok(()),
        -1 => Err(NativeError::InvalidConfiguration),
        -2 | -3 | -6 => Err(NativeError::Unavailable),
        -4 | -9 => Err(NativeError::Allocation),
        -5 => Err(NativeError::GeometryChanged),
        -7 => Err(NativeError::Closed),
        _ => Err(NativeError::Codec),
    }
}

/// One display as the capture system sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MacDisplay {
    pub id: u32,
    /// Origin in the global desktop, in this display's pixels.
    pub x: i32,
    pub y: i32,
    pub pixel_width: u32,
    pub pixel_height: u32,
    pub point_width: u32,
    pub point_height: u32,
}
impl MacDisplay {
    const fn raw(&self) -> RawDisplay {
        RawDisplay {
            display_id: self.id,
            x: self.x,
            y: self.y,
            pixel_width: self.pixel_width,
            pixel_height: self.pixel_height,
            point_width: self.point_width,
            point_height: self.point_height,
        }
    }

    /// `GeometryChanged` once the display is gone or its mode or placement
    /// differs from this description. Core Graphics only; cheap.
    pub fn check(&self) -> Result<(), NativeError> {
        let raw = self.raw();
        // SAFETY: a readable description for the duration of the call.
        status(unsafe { fr_cap_display_check(&raw const raw) })
    }

    /// The size the host encodes. The display's own pixels when they fit
    /// `limits`. Otherwise the largest of a few clean ratios that does: a
    /// clean ratio keeps text crisp, and 6144x3456 becomes 5120x2880 (5/6).
    /// `None` when even half the size does not fit.
    #[must_use]
    pub fn encoded_size(&self, limits: &ProtocolLimits) -> Option<(u32, u32)> {
        const RATIOS: [(u32, u32); 6] = [(1, 1), (5, 6), (4, 5), (3, 4), (2, 3), (1, 2)];
        RATIOS.iter().find_map(|&(n, d)| {
            let (w, h) = (self.pixel_width * n, self.pixel_height * n);
            if !w.is_multiple_of(d) || !h.is_multiple_of(d) {
                return None;
            }
            let (w, h) = (w / d, h / d);
            (w.is_multiple_of(2)
                && h.is_multiple_of(2)
                && limits.validate_coded_dimensions(w, h).is_ok())
            .then_some((w, h))
        })
    }
}

/// End this process when `expected` (its parent) exits: the macOS form of
/// Linux's parent-death signal. Refuses a process already reparented.
pub fn bind_parent(expected: u32) -> Result<(), NativeError> {
    let expected = i32::try_from(expected).map_err(|_| NativeError::InvalidConfiguration)?;
    // SAFETY: a scalar argument; the glue installs one process-exit watch.
    status(unsafe { fr_cap_bind_parent(expected) })
}

/// Displays the capture system can record. Unavailable without the Screen
/// Recording grant; this never prompts.
pub fn displays() -> Result<Vec<MacDisplay>, NativeError> {
    let mut raw = [RawDisplay::default(); 8];
    let mut count = 0;
    // SAFETY: the buffer and count are writable for the stated capacity.
    status(unsafe { fr_cap_displays(raw.as_mut_ptr(), raw.len(), &raw mut count) })?;
    Ok(raw[..count.min(raw.len())]
        .iter()
        .map(|d| MacDisplay {
            id: d.display_id,
            x: d.x,
            y: d.y,
            pixel_width: d.pixel_width,
            pixel_height: d.pixel_height,
            point_width: d.point_width,
            point_height: d.point_height,
        })
        .collect())
}

/// The result of one capture request.
#[derive(Debug)]
pub enum Captured {
    Unit(EncodedAccessUnit),
    Unchanged(UnchangedCapture),
}

/// Capture and encode of one display. Single owner, single thread.
pub struct MacCapture {
    raw: NonNull<c_void>,
    config: CodecConfiguration,
    limits: ProtocolLimits,
    admission: HevcGuard,
    buffer: Vec<u8>,
    last_output: Option<FrameId>,
    bitrate: u32,
    closed: bool,
}
// Bound for the first frame (capture start) and for one encode.
const ENCODE_TIMEOUT_MS: u32 = 1500;

impl MacCapture {
    pub fn open(
        display: u32,
        config: CodecConfiguration,
        limits: ProtocolLimits,
        fps: u32,
        bitrate: u32,
    ) -> Result<Self, NativeError> {
        let g = config.geometry();
        if config.color() != ColorInfo::sdr_bt709()
            || !(1..=240).contains(&fps)
            || !(10_000..=200_000_000).contains(&bitrate)
            || !g.crop_width().is_multiple_of(2)
            || !g.crop_height().is_multiple_of(2)
        {
            return Err(NativeError::InvalidConfiguration);
        }
        let admission =
            HevcGuard::new(config, limits, 4).map_err(|_| NativeError::InvalidConfiguration)?;
        let capacity = limits.max_encoded_access_unit_bytes() as usize;
        let mut buffer = Vec::new();
        buffer
            .try_reserve_exact(capacity)
            .map_err(|_| NativeError::Allocation)?;
        buffer.resize(capacity, 0);
        let mut raw = core::ptr::null_mut();
        // SAFETY: plain values and a writable out pointer.
        status(unsafe {
            fr_cap_open(
                display,
                g.crop_width(),
                g.crop_height(),
                fps,
                bitrate,
                &raw mut raw,
            )
        })?;
        Ok(Self {
            raw: NonNull::new(raw).ok_or(NativeError::Allocation)?,
            config,
            limits,
            admission,
            buffer,
            last_output: None,
            bitrate,
            closed: false,
        })
    }

    #[must_use]
    pub const fn bitrate(&self) -> u32 {
        self.bitrate
    }

    /// Encodes the newest frame as `frame`, stamped with `observed_micros`.
    /// With `only_if_changed` and nothing new on screen since the last unit,
    /// returns `Unchanged` evidence naming that unit instead. A failure
    /// closes the capture: no later request can splice a broken chain.
    pub fn capture(
        &mut self,
        frame: FrameId,
        observed_micros: u64,
        force_idr: bool,
        only_if_changed: bool,
    ) -> Result<Captured, NativeError> {
        if self.closed {
            return Err(NativeError::Closed);
        }
        let result = self.capture_inner(frame, observed_micros, force_idr, only_if_changed);
        if result.is_err() {
            self.closed = true;
        }
        result
    }

    fn capture_inner(
        &mut self,
        frame: FrameId,
        observed_micros: u64,
        force_idr: bool,
        only_if_changed: bool,
    ) -> Result<Captured, NativeError> {
        // A P picture needs a previous unit; the first is always an IDR.
        let force_idr = force_idr || self.last_output.is_none();
        if self.last_output.is_some_and(|last| frame <= last) {
            return Err(NativeError::StaleGeneration);
        }
        let (mut length, mut idr) = (0, 0);
        // SAFETY: unique live handle; the buffer is writable for its length.
        let code = unsafe {
            fr_cap_encode(
                self.raw.as_ptr(),
                c_int::from(force_idr),
                c_int::from(only_if_changed),
                ENCODE_TIMEOUT_MS,
                self.buffer.as_mut_ptr(),
                self.buffer.len(),
                &raw mut length,
                &raw mut idr,
            )
        };
        if code == UNCHANGED {
            let reference = self.last_output.ok_or(NativeError::Codec)?;
            return Ok(Captured::Unchanged(UnchangedCapture {
                candidate: frame,
                reference,
                observed_micros,
            }));
        }
        status(code)?;
        let idr = idr != 0;
        if force_idr && !idr {
            return Err(NativeError::UnsupportedBitstream);
        }
        let annex_b = strip_encoder_sei(&self.buffer[..length.min(self.buffer.len())])
            .map_err(|_| NativeError::UnsupportedBitstream)?;
        self.admission
            .validate_annex_b(&annex_b, idr)
            .map_err(|_| NativeError::UnsupportedBitstream)?;
        let bytes = annex_b_to_length_prefixed(&annex_b, self.limits)
            .map_err(|_| NativeError::UnsupportedBitstream)?;
        let kind = if idr {
            FrameKind::Idr {
                recovery: RecoveryGeneration::INITIAL,
            }
        } else {
            FrameKind::Predicted {
                references: self.last_output.ok_or(NativeError::UnsupportedBitstream)?,
            }
        };
        let unit = EncodedAccessUnit::new(
            &self.limits,
            frame,
            kind,
            self.config.generation(),
            observed_micros,
            bytes,
        )
        .map_err(|_| NativeError::Codec)?;
        self.last_output = Some(frame);
        Ok(Captured::Unit(unit))
    }

    /// Changes the encoder's average bitrate for later pictures.
    pub fn set_bitrate(&mut self, bitrate: u32) -> Result<(), NativeError> {
        if self.closed {
            return Err(NativeError::Closed);
        }
        if !(10_000..=200_000_000).contains(&bitrate) {
            return Err(NativeError::InvalidConfiguration);
        }
        // SAFETY: unique live handle.
        status(unsafe { fr_cap_set_bitrate(self.raw.as_ptr(), bitrate) })?;
        self.bitrate = bitrate;
        Ok(())
    }

    /// `GeometryChanged` once the stream stopped (display removed or
    /// reconfigured, or the Screen Recording grant revoked).
    pub fn check(&self) -> Result<(), NativeError> {
        if self.closed {
            return Err(NativeError::Closed);
        }
        // SAFETY: unique live handle.
        status(unsafe { fr_cap_check(self.raw.as_ptr()) })
    }
}
impl Drop for MacCapture {
    fn drop(&mut self) {
        // SAFETY: the handle came from fr_cap_open and is freed exactly once.
        unsafe { fr_cap_free(self.raw.as_ptr()) };
    }
}

#[cfg(test)]
mod tests {
    use super::MacDisplay;
    use fr_core::limits::ProtocolLimits;

    fn display(w: u32, h: u32) -> MacDisplay {
        MacDisplay {
            id: 1,
            x: 0,
            y: 0,
            pixel_width: w,
            pixel_height: h,
            point_width: w / 2,
            point_height: h / 2,
        }
    }

    #[test]
    fn encoded_size_is_native_when_it_fits_else_a_clean_ratio() {
        let limits = ProtocolLimits::ABSOLUTE;
        // 6K Retina exceeds the 16.78 MP cap: 5/6 keeps it crisp.
        assert_eq!(
            display(6144, 3456).encoded_size(&limits),
            Some((5120, 2880))
        );
        assert_eq!(
            display(5120, 2880).encoded_size(&limits),
            Some((5120, 2880))
        );
        assert_eq!(
            display(3456, 2234).encoded_size(&limits),
            Some((3456, 2234))
        );
        // 8K: 5/6, 4/5 and 3/4 all exceed the pixel cap; 2/3 fits.
        assert_eq!(
            display(7680, 4320).encoded_size(&limits),
            Some((5120, 2880))
        );
        assert_eq!(display(16, 16).encoded_size(&limits), Some((16, 16)));
    }
}
