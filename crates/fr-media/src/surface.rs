//! Opaque GPU surfaces and observable copy accounting (plan section 11.1).
//!
//! `FrankenRemote` keeps GPU surfaces opaque inside the backend adapter: this
//! crate never inspects pixels. A surface is identified by its backend, its
//! dimensions, and a pixel format; the actual memory lives behind the
//! [`GpuSurface`] trait. "Zero-copy" is a *measured* property, so every path
//! carries a [`CopyLedger`] that counts the copies it actually performed —
//! the contract makes copies observable rather than letting a backend hide
//! them.

use core::fmt;

/// Pixel formats a surface may carry across the capture -> convert -> encode
/// and decode -> present paths. The baseline is 4:2:0 for encode; BGRA/RGBA
/// appear at capture and presentation boundaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PixelFormat {
    /// 8-bit 4:2:0, the HEVC Main baseline encode format (NV12-class layout).
    Nv12,
    /// 8-bit packed BGRA, a common capture/presentation format.
    Bgra8,
    /// 8-bit packed RGBA.
    Rgba8,
}

impl PixelFormat {
    /// Bytes per pixel for the packed formats; `None` for planar/subsampled
    /// formats where a single bytes-per-pixel figure is meaningless.
    #[must_use]
    pub const fn packed_bytes_per_pixel(self) -> Option<u32> {
        match self {
            Self::Bgra8 | Self::Rgba8 => Some(4),
            Self::Nv12 => None,
        }
    }
}

impl fmt::Display for PixelFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Nv12 => "NV12",
            Self::Bgra8 => "BGRA8",
            Self::Rgba8 => "RGBA8",
        };
        f.write_str(s)
    }
}

/// Identifies which backend owns a surface, so a surface can never be handed
/// to a foreign backend by mistake (checked at admission/submit).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SurfaceBackend {
    /// Apple `VideoToolbox` / `IOSurface`.
    VideoToolbox,
    /// Direct3D 11 texture.
    Direct3D11,
    /// Linux DMA-BUF.
    DmaBuf,
    /// The in-memory test double.
    Fake,
}

/// The opaque GPU-surface contract. Implementors own the underlying memory and
/// release it at the platform's documented release point; this crate only ever
/// reads the identity and dimensions, never the pixels.
pub trait GpuSurface: fmt::Debug {
    /// Which backend owns this surface.
    fn backend(&self) -> SurfaceBackend;
    /// Pixel format of the surface.
    fn format(&self) -> PixelFormat;
    /// Visible width in pixels.
    fn width(&self) -> u32;
    /// Visible height in pixels.
    fn height(&self) -> u32;
}

/// A category of copy along a media path, for the [`CopyLedger`]. The point of
/// naming them is that a "slower" encoder that avoids an expensive transfer can
/// be the faster path overall (plan section 11.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CopyKind {
    /// A copy from a borrowed capture buffer into an owned GPU surface (done
    /// to release the compositor's buffer promptly).
    CaptureToOwned,
    /// A GPU-side color/format conversion that allocates a new surface.
    GpuConversion,
    /// A GPU-to-CPU readback — the most expensive kind; should be rare.
    GpuToCpuReadback,
    /// A CPU-to-GPU upload.
    CpuToGpuUpload,
}

/// Counts the copies performed along a path so they are observable rather than
/// hidden. A backend increments this as it works; diagnostics report it, and
/// a backend claiming "zero-copy" must show a zero ledger for the relevant
/// kinds on the qualified path.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CopyLedger {
    capture_to_owned: u32,
    gpu_conversion: u32,
    gpu_to_cpu_readback: u32,
    cpu_to_gpu_upload: u32,
}

impl CopyLedger {
    /// An empty ledger (no copies yet).
    #[must_use]
    pub const fn new() -> Self {
        Self {
            capture_to_owned: 0,
            gpu_conversion: 0,
            gpu_to_cpu_readback: 0,
            cpu_to_gpu_upload: 0,
        }
    }

    /// Records one copy of the given kind, saturating (a counter that pins at
    /// `u32::MAX` is a diagnostics signal, never a correctness input).
    pub fn record(&mut self, kind: CopyKind) {
        let slot = match kind {
            CopyKind::CaptureToOwned => &mut self.capture_to_owned,
            CopyKind::GpuConversion => &mut self.gpu_conversion,
            CopyKind::GpuToCpuReadback => &mut self.gpu_to_cpu_readback,
            CopyKind::CpuToGpuUpload => &mut self.cpu_to_gpu_upload,
        };
        *slot = slot.saturating_add(1);
    }

    /// Count for one copy kind.
    #[must_use]
    pub const fn count(&self, kind: CopyKind) -> u32 {
        match kind {
            CopyKind::CaptureToOwned => self.capture_to_owned,
            CopyKind::GpuConversion => self.gpu_conversion,
            CopyKind::GpuToCpuReadback => self.gpu_to_cpu_readback,
            CopyKind::CpuToGpuUpload => self.cpu_to_gpu_upload,
        }
    }

    /// Total copies of every kind.
    #[must_use]
    pub const fn total(&self) -> u32 {
        self.capture_to_owned
            .saturating_add(self.gpu_conversion)
            .saturating_add(self.gpu_to_cpu_readback)
            .saturating_add(self.cpu_to_gpu_upload)
    }

    /// True when no GPU-to-CPU readback occurred — the specific property a
    /// "kept pixels on the GPU" claim rests on.
    #[must_use]
    pub const fn is_readback_free(&self) -> bool {
        self.gpu_to_cpu_readback == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packed_formats_report_bytes_per_pixel() {
        assert_eq!(PixelFormat::Bgra8.packed_bytes_per_pixel(), Some(4));
        assert_eq!(PixelFormat::Rgba8.packed_bytes_per_pixel(), Some(4));
        assert_eq!(PixelFormat::Nv12.packed_bytes_per_pixel(), None);
    }

    #[test]
    fn copy_ledger_counts_by_kind_and_reports_readback_freedom() {
        let mut ledger = CopyLedger::new();
        assert!(ledger.is_readback_free());
        assert_eq!(ledger.total(), 0);
        ledger.record(CopyKind::CaptureToOwned);
        ledger.record(CopyKind::CaptureToOwned);
        ledger.record(CopyKind::GpuConversion);
        assert_eq!(ledger.count(CopyKind::CaptureToOwned), 2);
        assert_eq!(ledger.count(CopyKind::GpuConversion), 1);
        assert_eq!(ledger.total(), 3);
        assert!(ledger.is_readback_free());
        ledger.record(CopyKind::GpuToCpuReadback);
        assert!(!ledger.is_readback_free());
        assert_eq!(ledger.total(), 4);
    }
}
