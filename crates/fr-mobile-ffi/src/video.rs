//! `VideoToolbox` HEVC decode behind fr-media's HEVC guard, into a retained
//! IOSurface-backed NV12 pixel buffer per presented picture. Nothing is drawn
//! here: the newest picture waits in a [`FrameSlot`] until the app's Metal
//! view copies it (zero-copy, by reference) on its display refresh.
//!
//! The admission rules are fr-native's macOS viewer's: a record the guard
//! refuses never reaches `VideoToolbox`; units must follow the configured
//! generation and reference chain; a decode failure closes the decoder.
use core::ffi::{c_int, c_void};
use core::ptr::NonNull;
use fr_core::limits::ProtocolLimits;
use fr_media::access_unit::{EncodedAccessUnit, FrameId, FrameKind};
use fr_media::config::{CodecConfiguration, ColorRange};
use fr_media::hevc::HevcGuard;
use fr_media::worker::overlay::Update as CursorUpdate;
use std::sync::{
    Mutex,
    atomic::{AtomicU64, Ordering},
};

unsafe extern "C" {
    fn fr_vt_decoder_new(
        vps: *const u8,
        vps_len: usize,
        sps: *const u8,
        sps_len: usize,
        pps: *const u8,
        pps_len: usize,
        width: u32,
        height: u32,
        full_range: c_int,
        out: *mut *mut c_void,
    ) -> c_int;
    fn fr_vt_decoder_hardware(handle: *mut c_void) -> c_int;
    fn fr_vt_decoder_decode(
        handle: *mut c_void,
        bytes: *const u8,
        len: usize,
        pts: i64,
        image: *mut *mut c_void,
    ) -> c_int;
    fn fr_vt_decoder_free(handle: *mut c_void);
    fn fr_vt_image_retain(image: *mut c_void) -> *mut c_void;
    fn fr_vt_image_release(image: *mut c_void);
    fn fr_vt_image_describe(
        image: *mut c_void,
        width: *mut u32,
        height: *mut u32,
        format: *mut u32,
        io_surface: *mut c_int,
    ) -> c_int;
    fn fr_vt_image_mean_luma(image: *mut c_void, mean: *mut f64) -> c_int;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    InvalidConfiguration,
    Unavailable,
    Codec,
    Allocation,
    Geometry,
    SessionLost,
    StaleGeneration,
    UnsupportedBitstream,
    Closed,
}
fn status(code: c_int) -> Result<(), Error> {
    match code {
        0 => Ok(()),
        -1 => Err(Error::InvalidConfiguration),
        -2 => Err(Error::Unavailable),
        -4 => Err(Error::Allocation),
        -5 => Err(Error::Geometry),
        -6 => Err(Error::SessionLost),
        _ => Err(Error::Codec),
    }
}

/// One retained decoded picture (a `CVPixelBufferRef`).
pub struct Image(NonNull<c_void>);
// SAFETY: a CVPixelBuffer is a reference-counted CoreFoundation object whose
// retain/release are thread-safe; this owner holds exactly one +1 reference
// and never mutates the pixels.
unsafe impl Send for Image {}
impl Image {
    /// A new +1 reference for a caller that will release it (the app).
    pub fn retained_raw(&self) -> *mut c_void {
        // SAFETY: a live CVPixelBuffer this owner holds.
        unsafe { fr_vt_image_retain(self.0.as_ptr()) }
    }
    /// (width, height, `CoreVideo` pixel format, IOSurface-backed).
    pub fn describe(&self) -> (u32, u32, u32, bool) {
        let (mut w, mut h, mut f, mut s) = (0, 0, 0, 0);
        // SAFETY: a live CVPixelBuffer and four writable out values.
        let code = unsafe {
            fr_vt_image_describe(
                self.0.as_ptr(),
                &raw mut w,
                &raw mut h,
                &raw mut f,
                &raw mut s,
            )
        };
        if code == 0 {
            (w, h, f, s != 0)
        } else {
            (0, 0, 0, false)
        }
    }
    /// Mean of the luma plane (tests: a decoded picture, not a blank buffer).
    pub fn mean_luma(&self) -> Option<f64> {
        let mut mean = 0.0;
        // SAFETY: a live CVPixelBuffer and a writable out value.
        (unsafe { fr_vt_image_mean_luma(self.0.as_ptr(), &raw mut mean) } == 0).then_some(mean)
    }
}
impl Drop for Image {
    fn drop(&mut self) {
        // SAFETY: releases exactly the +1 reference this owner holds.
        unsafe { fr_vt_image_release(self.0.as_ptr()) };
    }
}

/// `kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange` ('420v').
pub const NV12_VIDEO_RANGE: u32 = 0x3432_3076;
/// `kCVPixelFormatType_420YpCbCr8BiPlanarFullRange` ('420f').
pub const NV12_FULL_RANGE: u32 = 0x3432_3066;

/// The VPS, SPS and PPS of a canonical record (`HevcGuard::decoder_record`
/// layout: 23-byte header, then three single-NAL arrays). Call only on a
/// record the guard admitted.
pub fn parameter_sets(record: &[u8]) -> Option<[&[u8]; 3]> {
    let mut offset: usize = 23;
    let mut sets: [&[u8]; 3] = [&[]; 3];
    for (slot, kind) in sets.iter_mut().zip(32_u8..=34) {
        let header = record.get(offset..offset.checked_add(5)?)?;
        if header[..3] != [0x80 | kind, 0, 1] {
            return None;
        }
        let length = usize::from(u16::from_be_bytes([header[3], header[4]]));
        offset += 5;
        *slot = record.get(offset..offset.checked_add(length)?)?;
        offset += length;
    }
    (offset == record.len()).then_some(sets)
}

/// `VideoToolbox` HEVC decode of admitted access units.
pub struct Decoder {
    raw: NonNull<c_void>,
    config: CodecConfiguration,
    limits: ProtocolLimits,
    admission: HevcGuard,
    last: Option<FrameId>,
    closed: bool,
    full_range: bool,
}
// SAFETY: the VideoToolbox session is used by one thread at a time (`&mut
// self`); its output callback runs synchronously inside decode.
unsafe impl Send for Decoder {}
impl Decoder {
    /// Admits `record` with the HEVC guard, then configures `VideoToolbox` from
    /// its parameter sets. A refused record never reaches `VideoToolbox`.
    pub fn new(
        config: CodecConfiguration,
        limits: ProtocolLimits,
        record: &[u8],
    ) -> Result<Self, Error> {
        let admission = HevcGuard::from_decoder_record(config, limits, 4, record)
            .map_err(|_| Error::InvalidConfiguration)?;
        let [vps, sps, pps] = parameter_sets(record).ok_or(Error::InvalidConfiguration)?;
        let g = config.geometry();
        let full_range = matches!(config.color().range, ColorRange::Full);
        let mut raw = core::ptr::null_mut();
        // SAFETY: admitted parameter-set slices live for the call and
        // CoreMedia copies them; the out pointer is valid.
        status(unsafe {
            fr_vt_decoder_new(
                vps.as_ptr(),
                vps.len(),
                sps.as_ptr(),
                sps.len(),
                pps.as_ptr(),
                pps.len(),
                g.crop_width(),
                g.crop_height(),
                c_int::from(full_range),
                &raw mut raw,
            )
        })?;
        Ok(Self {
            raw: NonNull::new(raw).ok_or(Error::Allocation)?,
            config,
            limits,
            admission,
            last: None,
            closed: false,
            full_range,
        })
    }
    /// Whether `VideoToolbox` chose a hardware decoder, when it says.
    pub fn hardware(&self) -> Option<bool> {
        // SAFETY: live decoder handle owned by this thread.
        match unsafe { fr_vt_decoder_hardware(self.raw.as_ptr()) } {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        }
    }
    /// The visible picture size (the encoded crop).
    pub fn picture(&self) -> (u32, u32) {
        let g = self.config.geometry();
        (g.crop_width(), g.crop_height())
    }
    pub const fn full_range(&self) -> bool {
        self.full_range
    }
    /// Admits and decodes one access unit; returns the picture when
    /// `present`. Admission and decode are one transaction: a refused or
    /// failed unit leaves the guard unchanged. A decode failure closes the
    /// decoder; recovery is a new IDR on a new one.
    pub fn submit(
        &mut self,
        unit: &EncodedAccessUnit,
        present: bool,
    ) -> Result<Option<Image>, Error> {
        if self.closed {
            return Err(Error::Closed);
        }
        if unit.config_generation() != self.config.generation()
            || unit.frame().as_raw() > i64::MAX as u64
            || self.last.is_some_and(|n| unit.frame() <= n)
        {
            return Err(Error::StaleGeneration);
        }
        self.limits
            .validate_access_unit_len(unit.bytes().len())
            .map_err(|_| Error::UnsupportedBitstream)?;
        let idr = unit.is_idr();
        if !idr
            && unit.kind()
                != (FrameKind::Predicted {
                    references: self.last.ok_or(Error::UnsupportedBitstream)?,
                })
        {
            return Err(Error::UnsupportedBitstream);
        }
        let mut admission = self.admission.clone();
        // Checks every NAL against the frozen parameter sets, then strips the
        // in-band copies: VideoToolbox decodes against the configured format.
        let sample = admission
            .prepare_hvc1(unit.bytes(), idr)
            .map_err(|_| Error::UnsupportedBitstream)?;
        let pts = i64::try_from(unit.capture_micros()).unwrap_or(i64::MAX);
        let bytes = sample.bytes();
        let mut image = core::ptr::null_mut();
        // SAFETY: the admitted sample lives for this synchronous call and the
        // glue copies it into a CoreMedia block buffer before decoding; the
        // out pointer receives a +1 picture only when requested.
        let result = status(unsafe {
            fr_vt_decoder_decode(
                self.raw.as_ptr(),
                bytes.as_ptr(),
                bytes.len(),
                pts,
                if present {
                    &raw mut image
                } else {
                    core::ptr::null_mut()
                },
            )
        });
        if let Err(error) = result {
            self.closed = true;
            return Err(error);
        }
        self.admission = admission;
        self.last = Some(unit.frame());
        Ok(NonNull::new(image).map(Image))
    }
}
impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: unique owner; invalidates the session before releasing it.
        unsafe { fr_vt_decoder_free(self.raw.as_ptr()) };
    }
}

/// The newest presented picture, for the app's renderer. A sequence number
/// lets the renderer skip pictures it already drew; the frame id lets it
/// report which picture actually reached the screen ([`FrameSlot::drawn`]).
#[derive(Default)]
pub struct FrameSlot {
    latest: Mutex<Option<Latest>>,
    sequence: AtomicU64,
    drawn: AtomicU64,
    presented: AtomicU64,
    decode_micros: AtomicU64,
    decoded: AtomicU64,
}
struct Latest {
    image: Image,
    sequence: u64,
    frame: u64,
}
/// What the app receives with a picture.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FrameInfo {
    pub sequence: u64,
    pub frame: u64,
    pub width: u32,
    pub height: u32,
    pub full_range: bool,
}
impl FrameSlot {
    /// Publish a presented picture (frd frame id `frame`).
    pub fn publish(&self, image: Image, frame: u64) {
        let sequence = self.sequence.fetch_add(1, Ordering::AcqRel) + 1;
        if let Ok(mut latest) = self.latest.lock() {
            *latest = Some(Latest {
                image,
                sequence,
                frame,
            });
        }
        self.presented.fetch_add(1, Ordering::Relaxed);
    }
    /// A new +1 reference to the newest picture when it is newer than
    /// `after`, with its info. The caller releases the reference.
    pub fn take_newer(&self, after: u64, full_range: bool) -> Option<(*mut c_void, FrameInfo)> {
        let latest = self.latest.lock().ok()?;
        let latest = latest.as_ref()?;
        if latest.sequence <= after {
            return None;
        }
        let (width, height, format, _) = latest.image.describe();
        Some((
            latest.image.retained_raw(),
            FrameInfo {
                sequence: latest.sequence,
                frame: latest.frame,
                width,
                height,
                full_range: full_range || format == NV12_FULL_RANGE,
            },
        ))
    }
    /// The app drew the picture with frd frame id `frame` into a presented
    /// drawable. Monotonic: an older report never lowers it.
    pub fn mark_drawn(&self, frame: u64) {
        self.drawn.fetch_max(frame, Ordering::AcqRel);
    }
    pub fn drawn(&self) -> u64 {
        self.drawn.load(Ordering::Acquire)
    }
    pub fn presented(&self) -> u64 {
        self.presented.load(Ordering::Relaxed)
    }
    /// Record one decode's wall time (content-free statistics).
    pub fn record_decode(&self, micros: u64) {
        self.decode_micros.fetch_add(micros, Ordering::Relaxed);
        self.decoded.fetch_add(1, Ordering::Relaxed);
    }
    /// (decoded units, total decode microseconds).
    pub fn decode_stats(&self) -> (u64, u64) {
        (
            self.decoded.load(Ordering::Relaxed),
            self.decode_micros.load(Ordering::Relaxed),
        )
    }
    /// Drop the retained picture (session end).
    pub fn clear(&self) {
        if let Ok(mut latest) = self.latest.lock() {
            *latest = None;
        }
    }
}

/// The client-rendered remote cursor (hosts that send it separately from the
/// picture), in decoded picture pixels. Shape bytes are RGBA, row-major.
#[derive(Default, Clone)]
pub struct Cursor {
    pub generation: u64,
    pub visible: bool,
    pub x: i32,
    pub y: i32,
    pub width: u16,
    pub height: u16,
    pub hotspot_x: u16,
    pub hotspot_y: u16,
    pub rgba: Vec<u8>,
}
impl Cursor {
    /// Apply one checked overlay update. A move before any shape, or an
    /// invalid shape, changes nothing and is refused.
    pub fn apply(&mut self, update: &CursorUpdate<'_>) -> Result<(), Error> {
        match *update {
            CursorUpdate::Hidden => self.visible = false,
            CursorUpdate::Move { x, y } => {
                if self.rgba.is_empty() {
                    return Err(Error::InvalidConfiguration);
                }
                self.x = x;
                self.y = y;
                self.visible = true;
            }
            CursorUpdate::Shape {
                x,
                y,
                width,
                height,
                hotspot_x,
                hotspot_y,
                rgba,
            } => {
                fr_media::cursor::check_geometry(width, height, hotspot_x, hotspot_y, rgba.len())
                    .map_err(|_| Error::InvalidConfiguration)?;
                self.x = x;
                self.y = y;
                self.width = width;
                self.height = height;
                self.hotspot_x = hotspot_x;
                self.hotspot_y = hotspot_y;
                self.rgba.clear();
                self.rgba.extend_from_slice(rgba);
                self.visible = true;
            }
        }
        self.generation += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
