//! Surface implementations implementing `fr_media::surface::GpuSurface` and `DecodedPicture`.

use alloc::vec::Vec;
use core::fmt;
use fr_media::codec::DecodedPicture;
use fr_media::surface::{GpuSurface, PixelFormat, SurfaceBackend};

/// Concrete surface type used across the `FFmpeg` boundary.
#[derive(Clone)]
pub struct FfiSurface {
    backend: SurfaceBackend,
    format: PixelFormat,
    width: u32,
    height: u32,
    data: Vec<u8>,
    opaque_handle: Option<u64>,
}

impl FfiSurface {
    /// Creates a new CPU-staged surface with owned pixel data.
    pub fn new(
        backend: SurfaceBackend,
        format: PixelFormat,
        width: u32,
        height: u32,
        data: Vec<u8>,
    ) -> Self {
        Self {
            backend,
            format,
            width,
            height,
            data,
            opaque_handle: None,
        }
    }

    /// Creates an opaque hardware texture surface.
    pub fn new_hardware(
        backend: SurfaceBackend,
        format: PixelFormat,
        width: u32,
        height: u32,
        handle: u64,
    ) -> Self {
        Self {
            backend,
            format,
            width,
            height,
            data: Vec::new(),
            opaque_handle: Some(handle),
        }
    }

    /// Access the underlying pixel bytes.
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// Mutable access to the underlying pixel bytes.
    pub fn data_mut(&mut self) -> &mut [u8] {
        &mut self.data
    }
}

impl fmt::Debug for FfiSurface {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FfiSurface")
            .field("backend", &self.backend)
            .field("format", &self.format)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("byte_len", &self.data.len())
            .field("opaque_handle", &self.opaque_handle)
            .finish()
    }
}

impl GpuSurface for FfiSurface {
    fn backend(&self) -> SurfaceBackend {
        self.backend
    }

    fn format(&self) -> PixelFormat {
        self.format
    }

    fn width(&self) -> u32 {
        self.width
    }

    fn height(&self) -> u32 {
        self.height
    }

    fn raw_bytes(&self) -> Option<&[u8]> {
        if self.data.is_empty() {
            None
        } else {
            Some(&self.data)
        }
    }

    fn opaque_handle(&self) -> Option<u64> {
        self.opaque_handle
    }

    fn as_any(&self) -> Option<&dyn core::any::Any> {
        Some(self)
    }
}

/// Decoded picture produced by `FfmpegDecoder`.
pub struct FfiDecodedPicture {
    frame_raw: u64,
    surface: FfiSurface,
}

impl FfiDecodedPicture {
    /// Creates a new decoded picture handle.
    pub fn new(frame_raw: u64, surface: FfiSurface) -> Self {
        Self { frame_raw, surface }
    }
}

impl fmt::Debug for FfiDecodedPicture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FfiDecodedPicture")
            .field("frame_raw", &self.frame_raw)
            .field("surface", &self.surface)
            .finish()
    }
}

impl DecodedPicture for FfiDecodedPicture {
    fn frame_raw(&self) -> u64 {
        self.frame_raw
    }

    fn surface(&self) -> &dyn GpuSurface {
        &self.surface
    }
}
