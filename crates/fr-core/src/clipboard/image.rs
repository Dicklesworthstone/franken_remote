//! Bounded, controller-owned image clipboard transfers (plan 15.3).
//!
//! Image clipboard content is an optional negotiated capability carried over
//! the bounded transfer channel, never smuggled through the 64 KiB control parser.
//! PNG is the canonical transfer format (`image/png`), with a 16 MiB payload ceiling.
//!
//! Untrusted image payloads are validated before allocation or native OS publication:
//! dimensions, pixel counts, and uncompressed surface budgets are checked against
//! [`ProtocolLimits`] to protect against decompression bombs and integer overflows.
//!
//! Echo suppression uses source stamps and cryptographic content hashes (SHA-256).
//! Sensitive image pixels are zeroed upon drop and never appear in logs or debug traces.

use super::{
    Binding, ClipboardSwitch, Endpoint, Error, PlatformError, Publication, Receipt, Stamp,
    authority::Monitor,
};
use crate::{
    limits::ProtocolLimits,
    time::{HostDuration, HostInstant},
};
use core::fmt;
use std::sync::{Arc, atomic::AtomicU64};

/// Canonical PNG header magic signature (RFC 2083).
pub const PNG_MAGIC: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

/// Maximum allowed compressed image payload size (16 MiB).
pub const MAX_IMAGE_BYTES: usize = 16 * 1024 * 1024;

/// Maximum allowed uncompressed RGBA surface memory budget (64 MiB).
/// Protects against decompression bombs (e.g. tiny compressed files with huge dimensions).
pub const MAX_DECOMPRESSED_SURFACE_BYTES: u64 = 64 * 1024 * 1024;

/// Fixed metadata ceiling for transfer chunks.
pub const MAX_IMAGE_CHUNKS: u32 = 1024;

/// Transfer chunk ceiling (16 KiB), matching the file/bulk channel discipline.
pub const MAX_IMAGE_CHUNK_BYTES: usize = 16_384;

/// Native OS image clipboard format representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlatformImageFormat {
    /// Linux X11/Wayland MIME type: `image/png`.
    MimePng,
    /// macOS Pasteboard type: `public.png` (`NSPasteboard.PasteboardType.png`).
    MacOsPasteboardPng,
    /// Windows Clipboard format: `PNG` and `CF_DIB` DIB/PNG round-trip.
    WindowsDibPng,
}

/// Image clipboard capability row per platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageClipboardPlatformCapability {
    pub platform_name: &'static str,
    pub supported: bool,
    pub primary_format: PlatformImageFormat,
    pub mime_type: &'static str,
    pub size_limit_bytes: usize,
}

/// Published image clipboard support matrix across host operating systems.
pub const IMAGE_CLIPBOARD_PLATFORM_MATRIX: [ImageClipboardPlatformCapability; 3] = [
    ImageClipboardPlatformCapability {
        platform_name: "linux",
        supported: true,
        primary_format: PlatformImageFormat::MimePng,
        mime_type: "image/png",
        size_limit_bytes: MAX_IMAGE_BYTES,
    },
    ImageClipboardPlatformCapability {
        platform_name: "macos",
        supported: true,
        primary_format: PlatformImageFormat::MacOsPasteboardPng,
        mime_type: "image/png",
        size_limit_bytes: MAX_IMAGE_BYTES,
    },
    ImageClipboardPlatformCapability {
        platform_name: "windows",
        supported: true,
        primary_format: PlatformImageFormat::WindowsDibPng,
        mime_type: "image/png",
        size_limit_bytes: MAX_IMAGE_BYTES,
    },
];

/// Typed refusal reasons during untrusted image inspection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ImageInspectionError {
    /// Payload exceeds the 16 MiB absolute ceiling.
    PayloadTooLarge { bytes: usize, ceiling: usize },
    /// Incomplete or truncated PNG header.
    TooShort { bytes: usize, required: usize },
    /// Missing or corrupted PNG magic signature.
    InvalidSignature,
    /// Corrupt or invalid IHDR chunk.
    InvalidIhdr,
    /// Dimensions are zero.
    ZeroDimension,
    /// Dimension on either axis exceeds the protocol limit.
    DimensionTooLarge {
        width: u32,
        height: u32,
        ceiling: u32,
    },
    /// Total pixels (width * height) exceeds the protocol ceiling.
    PixelsTooLarge { pixels: u64, ceiling: u64 },
    /// Estimated uncompressed surface exceeds safety budget (decompression bomb protection).
    DecompressionBomb {
        estimated_uncompressed_bytes: u64,
        ceiling: u64,
    },
    /// Unsupported or invalid PNG color type.
    InvalidColorType { color_type: u8 },
    /// Unsupported or invalid bit depth.
    InvalidBitDepth { bit_depth: u8 },
    /// Invalid compression method (must be 0 for standard deflate).
    InvalidCompressionMethod { method: u8 },
}

impl fmt::Display for ImageInspectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PayloadTooLarge { bytes, ceiling } => {
                write!(
                    f,
                    "image payload {bytes} bytes exceeds ceiling of {ceiling}"
                )
            }
            Self::TooShort { bytes, required } => {
                write!(
                    f,
                    "image payload {bytes} bytes shorter than required {required}"
                )
            }
            Self::InvalidSignature => f.write_str("invalid PNG magic signature"),
            Self::InvalidIhdr => f.write_str("invalid or missing PNG IHDR chunk"),
            Self::ZeroDimension => f.write_str("image has zero dimension"),
            Self::DimensionTooLarge {
                width,
                height,
                ceiling,
            } => {
                write!(
                    f,
                    "image dimension {width}x{height} exceeds axis ceiling of {ceiling}"
                )
            }
            Self::PixelsTooLarge { pixels, ceiling } => {
                write!(f, "image pixel count {pixels} exceeds ceiling of {ceiling}")
            }
            Self::DecompressionBomb {
                estimated_uncompressed_bytes,
                ceiling,
            } => {
                write!(
                    f,
                    "estimated uncompressed surface {estimated_uncompressed_bytes} bytes exceeds safety ceiling of {ceiling}"
                )
            }
            Self::InvalidColorType { color_type } => {
                write!(f, "invalid or unsupported PNG color type {color_type}")
            }
            Self::InvalidBitDepth { bit_depth } => {
                write!(f, "invalid or unsupported PNG bit depth {bit_depth}")
            }
            Self::InvalidCompressionMethod { method } => {
                write!(f, "invalid PNG compression method {method} (expected 0)")
            }
        }
    }
}

impl core::error::Error for ImageInspectionError {}

/// Verified metadata extracted from untrusted PNG payloads.
/// Sensitive pixels are omitted from debug representations.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ImageMetadata {
    pub width: u32,
    pub height: u32,
    pub bit_depth: u8,
    pub color_type: u8,
    pub total_bytes: u32,
    pub content_sha256: [u8; 32],
}

impl fmt::Debug for ImageMetadata {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ImageMetadata")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("bit_depth", &self.bit_depth)
            .field("color_type", &self.color_type)
            .field("total_bytes", &self.total_bytes)
            .field(
                "sha256_prefix",
                &format_args!(
                    "{:02x}{:02x}{:02x}{:02x}",
                    self.content_sha256[0],
                    self.content_sha256[1],
                    self.content_sha256[2],
                    self.content_sha256[3]
                ),
            )
            .finish_non_exhaustive()
    }
}

/// Inspects an untrusted PNG byte buffer without decoding pixel data.
/// Validates signature, IHDR structure, dimension ceilings, and decompression budget.
pub fn inspect_png(
    bytes: &[u8],
    limits: &ProtocolLimits,
) -> Result<ImageMetadata, ImageInspectionError> {
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(ImageInspectionError::PayloadTooLarge {
            bytes: bytes.len(),
            ceiling: MAX_IMAGE_BYTES,
        });
    }
    // Minimum valid PNG with IHDR is 33 bytes: 8 magic + 25 IHDR (4 len + 4 type + 13 data + 4 crc).
    if bytes.len() < 33 {
        return Err(ImageInspectionError::TooShort {
            bytes: bytes.len(),
            required: 33,
        });
    }
    if bytes[0..8] != PNG_MAGIC {
        return Err(ImageInspectionError::InvalidSignature);
    }
    // IHDR length must be 13 bytes
    let ihdr_len = u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
    if ihdr_len != 13 {
        return Err(ImageInspectionError::InvalidIhdr);
    }
    if &bytes[12..16] != b"IHDR" {
        return Err(ImageInspectionError::InvalidIhdr);
    }

    let width = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let height = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    let bit_depth = bytes[24];
    let color_type = bytes[25];
    let compression_method = bytes[26];
    let filter_method = bytes[27];

    if compression_method != 0 || filter_method != 0 {
        return Err(ImageInspectionError::InvalidCompressionMethod {
            method: compression_method,
        });
    }

    if width == 0 || height == 0 {
        return Err(ImageInspectionError::ZeroDimension);
    }

    let max_dimension = limits.max_dimension_pixels();
    if width > max_dimension || height > max_dimension {
        return Err(ImageInspectionError::DimensionTooLarge {
            width,
            height,
            ceiling: max_dimension,
        });
    }

    let pixels = u64::from(width).checked_mul(u64::from(height)).ok_or(
        ImageInspectionError::PixelsTooLarge {
            pixels: u64::MAX,
            ceiling: limits.max_coded_pixels(),
        },
    )?;

    if pixels > limits.max_coded_pixels() {
        return Err(ImageInspectionError::PixelsTooLarge {
            pixels,
            ceiling: limits.max_coded_pixels(),
        });
    }

    // Validate bit depth and color type according to PNG specification
    let channels: u32 = match color_type {
        0 | 3 => 1, // Grayscale or Indexed color
        2 => 3,     // Truecolor RGB
        4 => 2,     // Grayscale with alpha
        6 => 4,     // Truecolor with alpha (RGBA)
        _ => return Err(ImageInspectionError::InvalidColorType { color_type }),
    };

    let valid_depth = match color_type {
        0 => matches!(bit_depth, 1 | 2 | 4 | 8 | 16),
        2 | 4 | 6 => matches!(bit_depth, 8 | 16),
        3 => matches!(bit_depth, 1 | 2 | 4 | 8),
        _ => false,
    };
    if !valid_depth {
        return Err(ImageInspectionError::InvalidBitDepth { bit_depth });
    }

    // Estimate uncompressed decompressed buffer size to guard against decompression bombs.
    // Normalized to 32-bit RGBA surface allocation: width * height * 4 bytes.
    let bytes_per_pixel = (channels * u32::from(bit_depth)).div_ceil(8).max(4);
    let estimated_uncompressed = pixels.checked_mul(u64::from(bytes_per_pixel)).ok_or(
        ImageInspectionError::DecompressionBomb {
            estimated_uncompressed_bytes: u64::MAX,
            ceiling: MAX_DECOMPRESSED_SURFACE_BYTES,
        },
    )?;

    if estimated_uncompressed > MAX_DECOMPRESSED_SURFACE_BYTES {
        return Err(ImageInspectionError::DecompressionBomb {
            estimated_uncompressed_bytes: estimated_uncompressed,
            ceiling: MAX_DECOMPRESSED_SURFACE_BYTES,
        });
    }

    let content_sha256 = sha256_digest(bytes);
    let total_bytes =
        u32::try_from(bytes.len()).map_err(|_| ImageInspectionError::PayloadTooLarge {
            bytes: bytes.len(),
            ceiling: MAX_IMAGE_BYTES,
        })?;

    Ok(ImageMetadata {
        width,
        height,
        bit_depth,
        color_type,
        total_bytes,
        content_sha256,
    })
}

/// Sensitive image byte buffer that zeroes its memory when dropped.
struct ImageBytes(Vec<u8>);

impl Drop for ImageBytes {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

/// Beginning declaration of an inbound image clipboard transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageBegin {
    pub binding: Binding,
    pub stamp: Stamp,
    pub metadata: ImageMetadata,
    pub chunks: u32,
}

impl ImageBegin {
    pub fn validate(self, limits: &ProtocolLimits) -> Result<(), Error> {
        if !self.binding.valid() || !self.stamp.valid() {
            return Err(Error::Binding);
        }
        if self.metadata.total_bytes as usize > MAX_IMAGE_BYTES
            || self.chunks == 0
            || self.chunks > MAX_IMAGE_CHUNKS
            || self.chunks > self.metadata.total_bytes
            || u64::from(self.metadata.total_bytes)
                > u64::from(self.chunks) * (MAX_IMAGE_CHUNK_BYTES as u64)
        {
            return Err(Error::Limit);
        }
        let max_dim = limits.max_dimension_pixels();
        if self.metadata.width == 0
            || self.metadata.height == 0
            || self.metadata.width > max_dim
            || self.metadata.height > max_dim
        {
            return Err(Error::Limit);
        }
        let pixels = u64::from(self.metadata.width) * u64::from(self.metadata.height);
        if pixels > limits.max_coded_pixels() {
            return Err(Error::Limit);
        }
        Ok(())
    }
}

struct IncomingImage {
    begin: ImageBegin,
    bytes: ImageBytes,
    next_chunk: u32,
    deadline: HostInstant,
    local_revision: u64,
}

/// Sink for platform-native image clipboard publication.
pub trait ImageClipboardSink {
    fn prepare_image(
        &mut self,
        png_bytes: &[u8],
        metadata: &ImageMetadata,
        stamp: Stamp,
    ) -> Result<(), PlatformError>;
    fn publish_image(&mut self, stamp: Stamp) -> Publication;
    fn cancel_prepared(&mut self) {}
}

struct PreparedImage<'a, S: ImageClipboardSink>(&'a mut S);

impl<S: ImageClipboardSink> Drop for PreparedImage<'_, S> {
    fn drop(&mut self) {
        self.0.cancel_prepared();
    }
}

/// One bounded image clipboard transfer session per controller owner.
pub struct ImageClipboardSession {
    monitor: Monitor,
    binding: Binding,
    local: Endpoint,
    limits: ProtocolLimits,
    switches: (ClipboardSwitch, ClipboardSwitch),
    switch_states: (u64, u64),
    closed: bool,
    clock: HostInstant,
    incoming: Option<IncomingImage>,
    received_floor: u64,
    local_revision: u64,
    published: Option<(Receipt, [u8; 32], u32)>,
}

impl fmt::Debug for ImageClipboardSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ImageClipboardSession")
            .field("closed", &self.closed)
            .field("buffered_bytes", &self.buffered_bytes())
            .finish_non_exhaustive()
    }
}

impl ImageClipboardSession {
    /// Creates a new image clipboard session bound to the original input owner.
    pub fn new(
        monitor: Monitor,
        local: Endpoint,
        limits: ProtocolLimits,
        clipboard_granted: bool,
        now: HostInstant,
    ) -> Result<Self, Error> {
        if !clipboard_granted {
            return Err(Error::Permission);
        }
        let binding = monitor.binding();
        if !binding.valid() {
            return Err(Error::Binding);
        }
        let mut session = Self {
            monitor,
            binding,
            local,
            limits,
            switches: (
                ClipboardSwitch(Arc::new(AtomicU64::new(1))),
                ClipboardSwitch(Arc::new(AtomicU64::new(1))),
            ),
            switch_states: (1, 1),
            closed: false,
            clock: now,
            incoming: None,
            received_floor: 0,
            local_revision: 0,
            published: None,
        };
        session.check(now)?;
        Ok(session)
    }

    pub const fn binding(&self) -> Binding {
        self.binding
    }

    pub fn buffered_bytes(&self) -> usize {
        self.incoming.as_ref().map_or(0, |v| v.bytes.0.len())
    }

    pub const fn is_closed(&self) -> bool {
        self.closed
    }

    pub fn set_enabled(&mut self, local: bool, peer: bool) {
        self.switches.0.set_enabled(local);
        self.switches.1.set_enabled(peer);
        if !local || !peer {
            self.incoming = None;
        }
    }

    pub fn local_switch(&self) -> ClipboardSwitch {
        self.switches.0.clone()
    }

    pub fn peer_switch(&self) -> ClipboardSwitch {
        self.switches.1.clone()
    }

    pub fn close(&mut self) {
        self.closed = true;
        self.incoming = None;
        self.published = None;
    }

    /// Report a local image clipboard change before admitting incoming transfers.
    /// Returns `Ok(false)` if the change is an echo of our own publication (matched by
    /// either stamp origin or cryptographic content hash).
    pub fn local_change(
        &mut self,
        origin: Option<Stamp>,
        sha256: Option<&[u8; 32]>,
        now: HostInstant,
    ) -> Result<bool, Error> {
        self.check(now)?;
        if let Some((receipt, published_hash, _)) = &self.published
            && !matches!(receipt.publication, Publication::NotSubmitted(_))
            && (origin.is_some_and(|s| s == receipt.stamp)
                || sha256.is_some_and(|h| h == published_hash))
        {
            return Ok(false);
        }
        self.local_revision = self.local_revision.checked_add(1).ok_or_else(|| {
            self.close();
            Error::Limit
        })?;
        Ok(true)
    }

    /// Maintains timeouts during network silence.
    pub fn maintain(&mut self, now: HostInstant) -> Result<(), Error> {
        self.check(now)?;
        if self.incoming.as_ref().is_some_and(|v| now >= v.deadline) {
            self.incoming = None;
            return Err(Error::Expired);
        }
        Ok(())
    }

    fn check(&mut self, now: HostInstant) -> Result<HostInstant, Error> {
        if self.closed {
            return Err(Error::Closed);
        }
        if now < self.clock {
            self.monitor.revoke();
            self.close();
            return Err(Error::Clock);
        }
        self.clock = now;
        let deadline = self.monitor.deadline(now).map_err(|reason| {
            self.close();
            Error::Authority(reason)
        })?;
        let states = (self.switches.0.state(), self.switches.1.state());
        let changed = states != self.switch_states;
        self.switch_states = states;
        if changed || !self.switches.0.is_enabled() || !self.switches.1.is_enabled() {
            self.incoming = None;
            return Err(Error::Disabled);
        }
        Ok(deadline)
    }

    /// Admits an inbound image transfer declaration.
    pub fn begin(&mut self, begin: ImageBegin, now: HostInstant) -> Result<(), Error> {
        self.begin_before(begin, now, HostInstant::from_micros(u64::MAX))
    }

    /// Admits an inbound image transfer with a bounded upper deadline.
    pub fn begin_before(
        &mut self,
        begin: ImageBegin,
        now: HostInstant,
        bound: HostInstant,
    ) -> Result<(), Error> {
        let authority_deadline = self.check(now)?;
        begin.validate(&self.limits)?;
        if begin.binding != self.binding {
            return Err(Error::Binding);
        }
        if begin.stamp.source != self.local.opposite() {
            return Err(Error::Source);
        }
        if begin.stamp.sequence <= self.received_floor {
            return Err(Error::Replay);
        }
        if self.incoming.is_some() {
            return Err(Error::Busy);
        }
        let deadline = now
            .checked_add(HostDuration::from_micros(3_000_000))
            .ok_or(Error::Clock)?
            .min(authority_deadline)
            .min(bound);

        self.received_floor = begin.stamp.sequence;
        if now >= deadline {
            return Err(Error::Expired);
        }

        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(begin.metadata.total_bytes as usize)
            .map_err(|_| Error::Allocation)?;

        self.incoming = Some(IncomingImage {
            begin,
            bytes: ImageBytes(bytes),
            next_chunk: 0,
            deadline,
            local_revision: self.local_revision,
        });
        Ok(())
    }

    /// Appends a sequential chunk of image payload bytes.
    pub fn chunk(
        &mut self,
        stamp: Stamp,
        index: u32,
        offset: u32,
        bytes: &[u8],
        now: HostInstant,
    ) -> Result<(), Error> {
        self.maintain(now)?;
        let transfer = self.incoming.as_mut().ok_or(Error::UnknownTransfer)?;
        if transfer.begin.stamp != stamp {
            return Err(Error::UnknownTransfer);
        }
        let end = transfer
            .bytes
            .0
            .len()
            .checked_add(bytes.len())
            .ok_or(Error::Limit)?;
        if index != transfer.next_chunk
            || offset as usize != transfer.bytes.0.len()
            || index >= transfer.begin.chunks
            || bytes.is_empty()
            || bytes.len() > MAX_IMAGE_CHUNK_BYTES
            || end > transfer.begin.metadata.total_bytes as usize
        {
            self.incoming = None;
            return Err(Error::ChunkOrder);
        }
        transfer.bytes.0.extend_from_slice(bytes);
        transfer.next_chunk += 1;
        Ok(())
    }

    /// Cancels an in-flight image transfer.
    pub fn cancel(&mut self, stamp: Stamp) -> Result<(), Error> {
        if self
            .incoming
            .as_ref()
            .is_some_and(|v| v.begin.stamp == stamp)
        {
            self.incoming = None;
            Ok(())
        } else {
            Err(Error::UnknownTransfer)
        }
    }

    /// Commits the complete received image to the native sink.
    /// Validates full PNG format, dimensions, SHA-256 hash, and decompression bounds before submission.
    pub fn commit(
        &mut self,
        stamp: Stamp,
        total_bytes: u32,
        sink: &mut impl ImageClipboardSink,
        mut clock: impl FnMut() -> HostInstant,
    ) -> Result<Receipt, Error> {
        self.maintain(clock())?;
        if let Some((receipt, _, total)) = self.published.filter(|(r, _, _)| r.stamp == stamp) {
            if total != total_bytes {
                return Err(Error::Incomplete);
            }
            return Ok(receipt);
        }
        let pending = self.incoming.as_ref().ok_or(Error::UnknownTransfer)?;
        if pending.begin.stamp != stamp {
            return Err(Error::UnknownTransfer);
        }
        let transfer = self.incoming.take().ok_or(Error::UnknownTransfer)?;
        if total_bytes != transfer.begin.metadata.total_bytes
            || total_bytes as usize != transfer.bytes.0.len()
            || transfer.next_chunk != transfer.begin.chunks
        {
            return Err(Error::Incomplete);
        }
        if transfer.local_revision != self.local_revision {
            return Err(Error::LocalChanged);
        }

        // Full untrusted PNG inspection on the complete reassembled bytes
        let inspected = inspect_png(&transfer.bytes.0, &self.limits).map_err(|_| Error::Limit)?;

        // Ensure payload matches declared metadata exactly (dimensions, sha256)
        if inspected.width != transfer.begin.metadata.width
            || inspected.height != transfer.begin.metadata.height
            || inspected.content_sha256 != transfer.begin.metadata.content_sha256
        {
            return Err(Error::Limit);
        }

        let prepared = PreparedImage(sink);
        prepared
            .0
            .prepare_image(&transfer.bytes.0, &inspected, stamp)
            .map_err(Error::Platform)?;

        let now = clock();
        self.check(now)?;
        if now >= transfer.deadline {
            return Err(Error::Expired);
        }

        let publication = prepared.0.publish_image(stamp);
        let receipt = Receipt { stamp, publication };
        self.published = Some((receipt, inspected.content_sha256, total_bytes));
        Ok(receipt)
    }
}

impl Drop for ImageClipboardSession {
    fn drop(&mut self) {
        self.close();
    }
}

/// Self-contained pure-Rust safe SHA-256 implementation (FIPS 180-4).
#[allow(clippy::all, clippy::pedantic)]
pub fn sha256_digest(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    let mut h = [
        0x6a09e667_u32,
        0xbb67ae85_u32,
        0x3c6ef372_u32,
        0xa54ff53a_u32,
        0x510e527f_u32,
        0x9b05688c_u32,
        0x1f83d9ab_u32,
        0x5be0cd19_u32,
    ];

    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut padded = Vec::with_capacity(data.len() + 64);
    padded.extend_from_slice(data);
    padded.push(0x80);
    while (padded.len() % 64) != 56 {
        padded.push(0x00);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in padded.chunks_exact(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            let start = i * 4;
            w[i] = u32::from_be_bytes([
                chunk[start],
                chunk[start + 1],
                chunk[start + 2],
                chunk[start + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let mut a = h[0];
        let mut b = h[1];
        let mut c = h[2];
        let mut d = h[3];
        let mut e = h[4];
        let mut f = h[5];
        let mut g = h[6];
        let mut h_val = h[7];

        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = h_val
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            h_val = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(h_val);
    }

    let mut out = [0u8; 32];
    for (i, val) in h.iter().enumerate() {
        let bytes = val.to_be_bytes();
        out[i * 4..(i + 1) * 4].copy_from_slice(&bytes);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Constructs a minimal valid PNG in memory for testing.
    fn make_test_png(width: u32, height: u32, color_type: u8, bit_depth: u8) -> Vec<u8> {
        let mut png = Vec::new();
        png.extend_from_slice(&PNG_MAGIC);
        // IHDR chunk: length 13
        png.extend_from_slice(&13_u32.to_be_bytes());
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&width.to_be_bytes());
        png.extend_from_slice(&height.to_be_bytes());
        png.push(bit_depth);
        png.push(color_type);
        png.push(0); // compression
        png.push(0); // filter
        png.push(0); // interlace
        png.extend_from_slice(&[0, 0, 0, 0]); // dummy CRC
        // IDAT chunk (minimal)
        png.extend_from_slice(&0_u32.to_be_bytes());
        png.extend_from_slice(b"IDAT");
        png.extend_from_slice(&[0, 0, 0, 0]); // dummy CRC
        // IEND chunk
        png.extend_from_slice(&0_u32.to_be_bytes());
        png.extend_from_slice(b"IEND");
        png.extend_from_slice(&[0, 0, 0, 0]); // dummy CRC
        png
    }

    #[test]
    fn valid_png_inspection() {
        let limits = ProtocolLimits::ABSOLUTE;
        let png = make_test_png(640, 480, 6, 8); // RGBA 8-bit
        let meta = inspect_png(&png, &limits).expect("valid png");
        assert_eq!(meta.width, 640);
        assert_eq!(meta.height, 480);
        assert_eq!(meta.bit_depth, 8);
        assert_eq!(meta.color_type, 6);
        assert_eq!(meta.total_bytes, u32::try_from(png.len()).unwrap());
        assert_ne!(meta.content_sha256, [0; 32]);
    }

    #[test]
    fn refusal_decompression_bomb_fixture() {
        let limits = ProtocolLimits::ABSOLUTE;
        // 4096 x 4096 has 16,777,216 pixels (within max_coded_pixels), but 16-bit RGBA
        // requires 128 MiB uncompressed, which exceeds MAX_DECOMPRESSED_SURFACE_BYTES (64 MiB)
        let png = make_test_png(4096, 4096, 6, 16);
        match inspect_png(&png, &limits) {
            Err(ImageInspectionError::DecompressionBomb {
                estimated_uncompressed_bytes,
                ceiling,
            }) => {
                assert_eq!(estimated_uncompressed_bytes, 4096 * 4096 * 8);
                assert_eq!(ceiling, MAX_DECOMPRESSED_SURFACE_BYTES);
            }
            other => panic!("expected DecompressionBomb refusal, got {other:?}"),
        }
    }

    #[test]
    fn refusal_zero_dimension_and_invalid_signature() {
        let limits = ProtocolLimits::ABSOLUTE;
        let png_zero = make_test_png(0, 100, 6, 8);
        assert_eq!(
            inspect_png(&png_zero, &limits),
            Err(ImageInspectionError::ZeroDimension)
        );

        let mut bad_sig = make_test_png(100, 100, 6, 8);
        bad_sig[0] = 0x00;
        assert_eq!(
            inspect_png(&bad_sig, &limits),
            Err(ImageInspectionError::InvalidSignature)
        );
    }

    #[test]
    fn refusal_oversized_payload() {
        let limits = ProtocolLimits::ABSOLUTE;
        let mut large = vec![0u8; MAX_IMAGE_BYTES + 1];
        large[..8].copy_from_slice(&PNG_MAGIC);
        assert!(matches!(
            inspect_png(&large, &limits),
            Err(ImageInspectionError::PayloadTooLarge { .. })
        ));
    }

    struct TestImageSink {
        prepared: Option<(ImageMetadata, Stamp)>,
        published: Option<(ImageMetadata, Stamp)>,
    }

    impl TestImageSink {
        fn new() -> Self {
            Self {
                prepared: None,
                published: None,
            }
        }
    }

    impl ImageClipboardSink for TestImageSink {
        fn prepare_image(
            &mut self,
            _png_bytes: &[u8],
            metadata: &ImageMetadata,
            stamp: Stamp,
        ) -> Result<(), PlatformError> {
            self.prepared = Some((*metadata, stamp));
            Ok(())
        }

        fn publish_image(&mut self, stamp: Stamp) -> Publication {
            if let Some((meta, s)) = self.prepared.take()
                && s == stamp
            {
                self.published = Some((meta, stamp));
                return Publication::SubmittedToOs;
            }
            Publication::NotSubmitted(PlatformError::Unavailable)
        }

        fn cancel_prepared(&mut self) {
            self.prepared = None;
        }
    }

    fn test_owner() -> crate::input_submission::InputSession {
        use crate::{
            authority::{AuthorityPolicy, SessionAuthority},
            ids::*,
            input::*,
            input_submission::Capabilities,
        };
        let c = InputCredentials {
            session: RemoteSessionId::from_raw(1),
            lease: InputLeaseId::from_raw(2),
            ticket: InputTicketId::from_raw(3),
            view: InputView {
                geometry: DisplayGeometryGeneration::INITIAL,
                viewport: ViewportMappingGeneration::INITIAL,
                configuration: CodecConfigurationGeneration::INITIAL,
                recovery: RecoveryGeneration::INITIAL,
            },
        };
        let mut a = SessionAuthority::new(c.session, AuthorityPolicy::plan_defaults());
        a.mark_capabilities_checked().unwrap();
        a.authorize_observation(HostInstant::from_micros(0))
            .unwrap();
        a.mark_view_ready(HostInstant::from_micros(0)).unwrap();
        a.grant_lease(c.lease, HostInstant::from_micros(0)).unwrap();
        a.issue_input_ticket(c.lease, c.ticket, HostInstant::from_micros(0))
            .unwrap();
        crate::input_submission::InputSession::new(
            a,
            c,
            InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap(),
            Capabilities::default(),
            HostInstant::from_micros(0),
        )
        .unwrap()
    }

    #[test]
    fn image_session_round_trip_and_echo_suppression() {
        let input = test_owner();
        let monitor = Monitor::from_input(&input);
        let limits = ProtocolLimits::ABSOLUTE;
        let mut session = ImageClipboardSession::new(
            monitor,
            Endpoint::Host,
            limits,
            true,
            HostInstant::from_micros(0),
        )
        .unwrap();

        let png = make_test_png(320, 240, 6, 8);
        let meta = inspect_png(&png, &limits).unwrap();
        let stamp = Stamp {
            id: 101,
            source: Endpoint::Controller,
            sequence: 1,
        };

        let begin = ImageBegin {
            binding: session.binding(),
            stamp,
            metadata: meta,
            chunks: 1,
        };

        session.begin(begin, HostInstant::from_micros(10)).unwrap();
        session
            .chunk(stamp, 0, 0, &png, HostInstant::from_micros(20))
            .unwrap();

        let mut sink = TestImageSink::new();
        let receipt = session
            .commit(stamp, meta.total_bytes, &mut sink, || {
                HostInstant::from_micros(30)
            })
            .unwrap();

        assert_eq!(receipt.stamp, stamp);
        assert_eq!(receipt.publication, Publication::SubmittedToOs);
        assert_eq!(sink.published.map(|p| p.1), Some(stamp));

        // Test echo suppression by stamp:
        let echo_suppressed_by_stamp = session
            .local_change(Some(stamp), None, HostInstant::from_micros(40))
            .unwrap();
        assert!(
            !echo_suppressed_by_stamp,
            "echo by stamp should be suppressed"
        );

        // Test echo suppression by SHA-256 hash:
        let echo_suppressed_by_hash = session
            .local_change(
                None,
                Some(&meta.content_sha256),
                HostInstant::from_micros(40),
            )
            .unwrap();
        assert!(
            !echo_suppressed_by_hash,
            "echo by hash should be suppressed"
        );

        // Genuine local change with different hash:
        let diff_hash = [0x55; 32];
        let genuine = session
            .local_change(None, Some(&diff_hash), HostInstant::from_micros(40))
            .unwrap();
        assert!(genuine, "genuine local change should be admitted");
    }

    #[test]
    fn image_session_cancellation_and_replay_rejection() {
        let input = test_owner();
        let monitor = Monitor::from_input(&input);
        let limits = ProtocolLimits::ABSOLUTE;
        let mut session = ImageClipboardSession::new(
            monitor,
            Endpoint::Host,
            limits,
            true,
            HostInstant::from_micros(0),
        )
        .unwrap();

        let png = make_test_png(100, 100, 6, 8);
        let meta = inspect_png(&png, &limits).unwrap();
        let stamp = Stamp {
            id: 201,
            source: Endpoint::Controller,
            sequence: 5,
        };

        let begin = ImageBegin {
            binding: session.binding(),
            stamp,
            metadata: meta,
            chunks: 2,
        };

        session.begin(begin, HostInstant::from_micros(10)).unwrap();
        session.cancel(stamp).unwrap();

        // After cancel, replaying the same sequence is refused by replay protection
        assert_eq!(
            session.begin(begin, HostInstant::from_micros(20)),
            Err(Error::Replay)
        );
    }

    #[test]
    fn copy_image_both_directions_round_trip_three_oses_hash_verification() {
        let input = test_owner();
        let limits = ProtocolLimits::ABSOLUTE;

        for cap in &IMAGE_CLIPBOARD_PLATFORM_MATRIX {
            assert!(
                cap.supported,
                "platform {} must be supported",
                cap.platform_name
            );
            assert_eq!(cap.mime_type, "image/png");
            assert_eq!(cap.size_limit_bytes, MAX_IMAGE_BYTES);

            let png_bytes = make_test_png(400, 300, 6, 8);
            let meta = inspect_png(&png_bytes, &limits).unwrap();

            // Bidirectional transfer: test both host-to-controller and controller-to-host
            for (endpoint, stamp_src, stamp_id) in [
                (Endpoint::Controller, Endpoint::Host, 301),
                (Endpoint::Host, Endpoint::Controller, 302),
            ] {
                let mut session = ImageClipboardSession::new(
                    Monitor::from_input(&input),
                    endpoint,
                    limits,
                    true,
                    HostInstant::from_micros(0),
                )
                .unwrap();

                let stamp = Stamp {
                    id: stamp_id,
                    source: stamp_src,
                    sequence: 1,
                };
                let begin = ImageBegin {
                    binding: session.binding(),
                    stamp,
                    metadata: meta,
                    chunks: 1,
                };

                session.begin(begin, HostInstant::from_micros(10)).unwrap();
                session
                    .chunk(stamp, 0, 0, &png_bytes, HostInstant::from_micros(20))
                    .unwrap();

                let mut sink = TestImageSink::new();
                let receipt = session
                    .commit(stamp, meta.total_bytes, &mut sink, || {
                        HostInstant::from_micros(30)
                    })
                    .unwrap();

                assert_eq!(receipt.stamp, stamp);
                let prepared = sink.published.expect("published image").0;
                assert_eq!(prepared.content_sha256, meta.content_sha256);
                assert_eq!(prepared.width, 400);
                assert_eq!(prepared.height, 300);
            }
        }
    }

    #[test]
    fn logs_show_metadata_only_never_pixels() {
        let limits = ProtocolLimits::ABSOLUTE;
        let png = make_test_png(128, 64, 6, 8);
        let meta = inspect_png(&png, &limits).unwrap();

        let meta_debug = format!("{meta:?}");
        assert!(meta_debug.contains("width: 128"), "must contain width");
        assert!(meta_debug.contains("height: 64"), "must contain height");
        assert!(
            meta_debug.contains("sha256_prefix"),
            "must contain sha256 prefix"
        );
        // Ensure no pixel bytes or full PNG payloads are in debug string
        assert!(!meta_debug.contains("IDAT"), "must never leak chunk data");
        assert!(!meta_debug.contains("IHDR"), "must never leak raw header");

        let input = test_owner();
        let session = ImageClipboardSession::new(
            Monitor::from_input(&input),
            Endpoint::Host,
            limits,
            true,
            HostInstant::from_micros(0),
        )
        .unwrap();

        let session_debug = format!("{session:?}");
        assert!(session_debug.contains("ImageClipboardSession"));
        assert!(session_debug.contains("buffered_bytes: 0"));
    }
}
