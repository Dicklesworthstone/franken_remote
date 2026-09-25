//! Measured X11 pixel-transfer work. No codec/GPU/server-internal copy claims.
use crate::NativeError;
use core::ffi::c_void;

/// Why a surface selected the socket path. Selection is sticky for that owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fallback {
    ExtensionUnavailable,
    DescriptorTransportUnavailable,
    SharedAllocationUnavailable,
    AttachmentRefused,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Path {
    NotInitialized,
    SharedMemory,
    Socket(Fallback),
}
/// Counts saturate at `u64::MAX`, never wrap to a smaller apparent workload.
/// Retention counts the image's backing bytes; fixed native metadata is extra.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Statistics {
    pub path: Path,
    pub images: u64,
    pub copied_bytes: u64,
    pub socket_pixel_bytes: u64,
    pub retained_bytes: u64,
}
#[repr(C)]
#[derive(Default)]
pub(crate) struct Raw {
    path: u32,
    fallback: u32,
    images: u64,
    copied_bytes: u64,
    socket_pixel_bytes: u64,
    retained_bytes: u64,
}
impl Raw {
    pub(crate) fn decode(self) -> Result<Statistics, NativeError> {
        let path = match (self.path, self.fallback) {
            (0, 0) => Path::NotInitialized,
            (1, 0) => Path::SharedMemory,
            (2, reason) => Path::Socket(match reason {
                1 => Fallback::ExtensionUnavailable,
                2 => Fallback::DescriptorTransportUnavailable,
                3 => Fallback::SharedAllocationUnavailable,
                4 => Fallback::AttachmentRefused,
                _ => return Err(NativeError::InvalidConfiguration),
            }),
            _ => return Err(NativeError::InvalidConfiguration),
        };
        Ok(Statistics {
            path,
            images: self.images,
            copied_bytes: self.copied_bytes,
            socket_pixel_bytes: self.socket_pixel_bytes,
            retained_bytes: self.retained_bytes,
        })
    }
}
unsafe extern "C" {
    pub(crate) fn fr_x11_transfer_stats(surface: *mut c_void, out: *mut Raw);
    pub(crate) fn fr_ximage_stats(capture: *mut c_void, out: *mut Raw);
    pub(crate) fn fr_ximage_free(capture: *mut c_void);
}
