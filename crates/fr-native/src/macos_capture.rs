//! macOS host capture (fr-p1-host-macos-vif): `ScreenCaptureKit` frames go
//! straight into a `VideoToolbox` HEVC session (`macos_capture.m`), without an
//! application-side full-frame CPU copy. Every access unit is stripped of
//! encoder SEI, admitted by the same strict `HevcGuard` as the Linux encoders,
//! and leaves as canonical four-byte-length NALs.
//!
//! Source evidence is the capture system's own: only a post-request Complete
//! or Idle event can justify the broker's pre-request timestamp. Silence and
//! cached pixels never become fresh evidence. When no such event arrives,
//! macOS 14+ performs a bounded real screenshot through the original filter
//! and configuration, then encodes that verification refresh. Fresh Idle events
//! can suppress encoding; verification captures have an explicit counted cost.
use crate::{NativeError, capture_rate::Rate};
use core::ffi::{c_int, c_void};
use core::ptr::NonNull;
use fr_core::{ids::RecoveryGeneration, limits::ProtocolLimits};
use fr_media::{
    access_unit::{EncodedAccessUnit, FrameId, FrameKind},
    config::{CodecConfiguration, ColorInfo},
    hevc::HevcGuard,
    worker::UnchangedCapture,
};
use std::time::Instant;

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
        expected: *const RawDisplay,
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
        target_bitrate: u32,
        actual_bitrate: *mut u32,
    ) -> c_int;
    fn fr_cap_check(handle: *mut c_void) -> c_int;
    fn fr_cap_statistics(
        handle: *mut c_void,
        requests: *mut u64,
        encoded_refreshes: *mut u64,
        verification_wait_ms: *mut u32,
    ) -> c_int;
    fn fr_cap_free(handle: *mut c_void);
    fn fr_cap_display_check(expected: *const RawDisplay) -> c_int;
    fn fr_cap_bind_parent(expected: i32) -> c_int;
    fn fr_cap_request_access() -> c_int;
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

pub use crate::macos_display::MacDisplay;

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
    /// differs from this description. Core Graphics queries, each a round
    /// trip to the window server: slow when it is busy.
    pub fn check(&self) -> Result<(), NativeError> {
        let raw = self.raw();
        // SAFETY: a readable description for the duration of the call.
        status(unsafe { fr_cap_display_check(&raw const raw) })
    }
}

/// Ask macOS for Screen Recording on behalf of this process's responsible
/// process (frd under launchd, so the prompt and the System Settings entry
/// name frd). The system prompts only while no decision is recorded;
/// afterwards this only reports. Startup probes only, never a live share.
pub fn request_access() -> bool {
    // SAFETY: a scalar query with no arguments.
    unsafe { fr_cap_request_access() != 0 }
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

/// Content-free work counters for the original capture owner. These are API
/// requests and encoded units, not measured GPU time or zero-copy evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureStatistics {
    /// Real screenshot API requests, including ones that later fail or expire.
    pub verification_requests: u64,
    /// Encoded units whose retained pixels came from verification capture.
    pub verification_encoded: u64,
    /// Silence before requesting verification, also bounding its request rate.
    pub verification_wait_ms: u32,
}

/// Capture and encode of one display. Single owner, single thread.
/// Requires macOS 14+ for real source verification during stream silence.
pub struct MacCapture {
    raw: NonNull<c_void>,
    config: CodecConfiguration,
    limits: ProtocolLimits,
    admission: HevcGuard,
    buffer: Vec<u8>,
    last_output: Option<FrameId>,
    last_request: Option<FrameId>,
    last_observed: Option<u64>,
    bitrate: u32,
    ceiling_bitrate: u32,
    rate: Rate,
    closed: bool,
}
// Bound for the first frame (capture start) and for one encode.
const ENCODE_TIMEOUT_MS: u32 = 1500;

impl MacCapture {
    /// Capture `display` as described: the open is refused with
    /// `GeometryChanged` if its mode or placement no longer match, and every
    /// later live check compares against this same description.
    pub fn open(
        display: &MacDisplay,
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
        let expected = display.raw();
        // SAFETY: a readable description for the call, plain values and a
        // writable out pointer.
        status(unsafe {
            fr_cap_open(
                &raw const expected,
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
            last_request: None,
            last_observed: None,
            bitrate,
            ceiling_bitrate: bitrate,
            rate: Rate::new(bitrate),
            closed: false,
        })
    }

    #[must_use]
    pub const fn bitrate(&self) -> u32 {
        self.bitrate
    }

    /// Captures source evidence after this request and encodes it as `frame`.
    /// `observed_micros` is the parent's pre-request lower bound, never reply
    /// arrival time. Only a fresh OS Idle observation of the encoded picture
    /// may return `Unchanged`; silence causes real verification capture.
    /// A failure closes the capture: no later request can splice a broken chain.
    pub fn capture(
        &mut self,
        frame: FrameId,
        observed_micros: u64,
        force_idr: bool,
        only_if_changed: bool,
    ) -> Result<Captured, NativeError> {
        // Legacy capture explicitly cancels an uncommitted adaptive target.
        self.capture_request(
            frame,
            observed_micros,
            force_idr,
            only_if_changed,
            self.bitrate,
        )
    }

    /// Retain a rate target on the original capture owner. Fresh Idle evidence
    /// still returns Unchanged at the previous rate. A real required encode may
    /// apply the target only after the same dual-clock dwell as software capture;
    /// its property readback and admitted IDR precede the reported rate change.
    pub fn capture_with_bitrate(
        &mut self,
        frame: FrameId,
        observed_micros: u64,
        force_idr: bool,
        only_if_changed: bool,
        bitrate: u32,
    ) -> Result<Captured, NativeError> {
        self.capture_request(frame, observed_micros, force_idr, only_if_changed, bitrate)
    }

    fn capture_request(
        &mut self,
        frame: FrameId,
        observed_micros: u64,
        force_idr: bool,
        only_if_changed: bool,
        bitrate: u32,
    ) -> Result<Captured, NativeError> {
        if self.closed {
            return Err(NativeError::Closed);
        }
        if !(10_000..=self.ceiling_bitrate).contains(&bitrate) {
            return Err(NativeError::InvalidConfiguration);
        }
        if self.last_request.is_some_and(|last| frame <= last)
            || self
                .last_observed
                .is_some_and(|last| observed_micros < last)
        {
            self.closed = true;
            return Err(NativeError::StaleGeneration);
        }
        self.last_request = Some(frame);
        self.last_observed = Some(observed_micros);
        self.rate.request(bitrate);
        let target = self
            .rate
            .due(observed_micros, Instant::now(), self.last_output.is_some())
            .unwrap_or(self.bitrate);
        let result = self.capture_inner(frame, observed_micros, force_idr, only_if_changed, target);
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
        target_bitrate: u32,
    ) -> Result<Captured, NativeError> {
        // A P picture needs a previous unit; the first is always an IDR.
        let force_idr = force_idr || self.last_output.is_none();
        let (mut length, mut idr) = (0, 0);
        let mut actual_bitrate = 0;
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
                target_bitrate,
                &raw mut actual_bitrate,
            )
        };
        if code == UNCHANGED {
            let reference = self.last_output.ok_or(NativeError::Codec)?;
            if force_idr || !only_if_changed || actual_bitrate != self.bitrate {
                return Err(NativeError::UnsupportedBitstream);
            }
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
        let encoded = self
            .buffer
            .get(..length)
            .filter(|bytes| !bytes.is_empty())
            .ok_or(NativeError::UnsupportedBitstream)?;
        let bytes = self
            .admission
            .normalize_encoder_length_prefixed(encoded, idr)
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
        self.rate
            .confirm(
                target_bitrate,
                actual_bitrate,
                idr,
                observed_micros,
                Instant::now(),
            )
            .map_err(|()| NativeError::UnsupportedBitstream)?;
        self.bitrate = actual_bitrate;
        self.last_output = Some(frame);
        Ok(Captured::Unit(unit))
    }

    /// Cumulative native verification work, without reading pixels or creating
    /// source evidence. Available even after a refused request for diagnostics.
    pub fn statistics(&self) -> Result<CaptureStatistics, NativeError> {
        let (mut requests, mut encoded_refreshes, mut verification_wait_ms) = (0, 0, 0);
        // SAFETY: live thread-confined owner and writable scalar outputs.
        status(unsafe {
            fr_cap_statistics(
                self.raw.as_ptr(),
                &raw mut requests,
                &raw mut encoded_refreshes,
                &raw mut verification_wait_ms,
            )
        })?;
        Ok(CaptureStatistics {
            verification_requests: requests,
            verification_encoded: encoded_refreshes,
            verification_wait_ms,
        })
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
