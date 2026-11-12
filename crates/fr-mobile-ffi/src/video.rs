//! Platform HEVC decode behind fr-media's HEVC guard: `VideoToolbox` into a
//! retained IOSurface-backed NV12 pixel buffer per presented picture on
//! Apple, Media Foundation into an NV12 D3D11 texture on Windows. Nothing is
//! drawn here: the newest picture waits in a [`FrameSlot`] until the app's
//! renderer takes it (by reference) on its display refresh.
//!
//! The admission rules are fr-native's macOS viewer's: a record the guard
//! refuses never reaches `VideoToolbox`; units must follow the configured
//! generation and reference chain; a decode failure closes the decoder.
use core::ffi::c_void;
use fr_core::limits::ProtocolLimits;
use fr_media::access_unit::{EncodedAccessUnit, FrameId, FrameKind};
use fr_media::config::{CodecConfiguration, ColorRange};
use fr_media::hevc::HevcGuard;
use fr_media::worker::overlay::Update as CursorUpdate;
use std::sync::{
    Mutex,
    atomic::{AtomicU64, Ordering},
};

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
// The platform decoder: `VideoToolbox` on Apple, Media Foundation on Windows.
#[cfg(any(target_os = "ios", target_os = "macos"))]
#[path = "video/apple.rs"]
mod backend;
#[cfg(windows)]
#[path = "video/windows.rs"]
mod backend;
pub use backend::Image;
/// The Windows client presents from the decoder's own device.
#[cfg(windows)]
pub use backend::{Gpu, gpu, texture_of};

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

/// Platform HEVC decode of admitted access units.
pub struct Decoder {
    raw: backend::RawDecoder,
    config: CodecConfiguration,
    limits: ProtocolLimits,
    admission: HevcGuard,
    last: Option<FrameId>,
    closed: bool,
    full_range: bool,
}
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
        let raw =
            backend::RawDecoder::new([vps, sps, pps], g.crop_width(), g.crop_height(), full_range)?;
        Ok(Self {
            raw,
            config,
            limits,
            admission,
            last: None,
            closed: false,
            full_range,
        })
    }
    /// Whether the platform chose a hardware decoder, when it says.
    pub fn hardware(&self) -> Option<bool> {
        self.raw.hardware()
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
        let image = match self.raw.decode(sample.bytes(), idr, pts, present) {
            Ok(image) => image,
            Err(error) => {
                self.closed = true;
                return Err(error);
            }
        };
        self.admission = admission;
        self.last = Some(unit.frame());
        Ok(image)
    }
}

/// The newest presented picture, for the app's renderer. A sequence number
/// lets the renderer skip pictures it already drew; the frame id lets it
/// report which picture actually reached the screen ([`FrameSlot::drawn`]).
///
/// frd numbers frames per capture source, so a reconnect can start again
/// from zero. The id the app sees carries the attempt's generation in its top
/// bits ([`FrameSlot::app_id`]): a picture drawn in an earlier attempt can
/// never stand for one of this attempt.
#[derive(Default)]
pub struct FrameSlot {
    latest: Mutex<Option<Latest>>,
    sequence: AtomicU64,
    generation: AtomicU64,
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
/// Bits of an app-visible frame id that hold frd's own frame id (2^44
/// frames: centuries at any frame rate); the rest is the attempt generation.
const RAW_BITS: u32 = 44;
const RAW_MASK: u64 = (1 << RAW_BITS) - 1;

impl FrameSlot {
    /// A new connection attempt: later ids are all above every earlier one.
    pub fn begin_attempt(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
    }
    /// The app-visible id of frd frame `raw` in the current attempt.
    pub fn app_id(&self, raw: u64) -> u64 {
        (self.generation.load(Ordering::Acquire) << RAW_BITS) | (raw & RAW_MASK)
    }
    /// Publish a presented picture (frd frame id `frame`).
    pub fn publish(&self, image: Image, frame: u64) {
        let sequence = self.sequence.fetch_add(1, Ordering::AcqRel) + 1;
        let frame = self.app_id(frame);
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
    /// The app drew the picture with app-visible id `frame` (from
    /// [`FrameInfo::frame`]) into a presented drawable. Monotonic: an older
    /// report never lowers it.
    pub fn mark_drawn(&self, frame: u64) {
        self.drawn.fetch_max(frame, Ordering::AcqRel);
    }
    /// The newest app-visible id reported drawn.
    pub fn drawn(&self) -> u64 {
        self.drawn.load(Ordering::Acquire)
    }
    /// frd's frame id within [`Self::drawn`] (content-free statistics).
    pub fn drawn_raw(&self) -> u64 {
        self.drawn() & RAW_MASK
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

#[cfg(all(test, any(target_os = "ios", target_os = "macos", windows)))]
mod tests;
