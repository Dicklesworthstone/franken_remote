//! `FFmpeg` HEVC decoder wrapper implementing `fr_media::codec::Decoder`.

use alloc::boxed::Box;
use alloc::vec;
use core::ffi::c_int;
use core::marker::PhantomData;
use core::ptr::NonNull;

use fr_core::limits::ProtocolLimits;
use fr_media::access_unit::EncodedAccessUnit;
use fr_media::codec::{DecodedPicture, Decoder, MediaError};
use fr_media::config::CodecConfiguration;
use fr_media::surface::{PixelFormat, SurfaceBackend};

use crate::error::{map_receive_status, map_send_status};
use crate::surface::{FfiDecodedPicture, FfiSurface};

#[repr(C)]
struct FrFfiDecoder {
    _opaque: [u8; 0],
}

unsafe extern "C" {
    fn fr_ffi_decoder_new(
        width: c_int,
        height: c_int,
        extradata: *const u8,
        extradata_len: usize,
        out: *mut *mut FrFfiDecoder,
    ) -> c_int;

    fn fr_ffi_decoder_send_packet(
        dec: *mut FrFfiDecoder,
        data: *const u8,
        len: usize,
        pts: u64,
    ) -> c_int;

    fn fr_ffi_decoder_receive_frame(
        dec: *mut FrFfiDecoder,
        out_bgra: *mut u8,
        out_capacity: usize,
        out_pts: *mut u64,
    ) -> c_int;

    fn fr_ffi_decoder_drain(dec: *mut FrFfiDecoder) -> c_int;

    fn fr_ffi_decoder_free(dec: *mut FrFfiDecoder);
}

/// `FFmpeg`-backed HEVC decoder implementing `fr_media::codec::Decoder`.
///
/// Thread-confined: non-`Sync`, isolated from the authority/event-loop thread.
pub struct FfmpegDecoder {
    raw: Option<NonNull<FrFfiDecoder>>,
    config: Option<CodecConfiguration>,
    limits: ProtocolLimits,
    is_mock: bool,
    expected_backend: SurfaceBackend,
    _marker: PhantomData<*mut ()>,
}

// SAFETY: Send is safe because unique ownership is transferred across threads.
// Sync is forbidden because FFmpeg contexts are not thread-safe.
unsafe impl Send for FfmpegDecoder {}

impl FfmpegDecoder {
    /// Creates a new unconfigured decoder instance.
    pub fn new(expected_backend: SurfaceBackend) -> Self {
        Self {
            raw: None,
            config: None,
            limits: ProtocolLimits::ABSOLUTE,
            is_mock: false,
            expected_backend,
            _marker: PhantomData,
        }
    }

    /// Creates a mock decoder for testing.
    pub fn new_mock(expected_backend: SurfaceBackend) -> Self {
        Self {
            raw: None,
            config: None,
            limits: ProtocolLimits::ABSOLUTE,
            is_mock: true,
            expected_backend,
            _marker: PhantomData,
        }
    }

    /// Flushes and drains any pending decoded frames.
    pub fn drain(&mut self) -> Result<(), MediaError> {
        let raw = self.raw.ok_or(MediaError::NotConfigured)?;
        let res = unsafe { fr_ffi_decoder_drain(raw.as_ptr()) };
        map_send_status(res)
    }
}

impl Decoder for FfmpegDecoder {
    fn configure(&mut self, config: CodecConfiguration) -> Result<(), MediaError> {
        // Clean up any previously opened context
        if let Some(raw) = self.raw.take() {
            unsafe {
                fr_ffi_decoder_free(raw.as_ptr());
            }
        }

        let geom = config.geometry();
        let width = geom.crop_width().cast_signed();
        let height = geom.crop_height().cast_signed();

        let mut out_ptr: *mut FrFfiDecoder = core::ptr::null_mut();

        let res = if self.is_mock {
            unsafe { fr_ffi_decoder_new(width, height, core::ptr::null(), 0, &raw mut out_ptr) }
        } else {
            // Supply empty or canonical extradata; real streams pass parameter sets in-band or via hvcC
            unsafe { fr_ffi_decoder_new(width, height, core::ptr::null(), 0, &raw mut out_ptr) }
        };

        if res != 0 {
            return match res {
                crate::error::FR_FFI_DEVICE_LOST => Err(MediaError::DeviceLost),
                crate::error::FR_FFI_INVALID => Err(MediaError::ConfigMismatch),
                _ => Err(MediaError::Fatal),
            };
        }

        self.raw = NonNull::new(out_ptr);
        self.config = Some(config);
        Ok(())
    }

    fn submit(&mut self, access_unit: &EncodedAccessUnit) -> Result<(), MediaError> {
        let config = self.config.ok_or(MediaError::NotConfigured)?;
        let raw = self.raw.ok_or(MediaError::NotConfigured)?;

        if access_unit.config_generation() != config.generation() {
            return Err(MediaError::ConfigMismatch);
        }

        let bytes = access_unit.bytes();
        self.limits
            .validate_access_unit_len(bytes.len())
            .map_err(|_| MediaError::Fatal)?;

        let pts = access_unit.frame().as_raw();
        let res =
            unsafe { fr_ffi_decoder_send_packet(raw.as_ptr(), bytes.as_ptr(), bytes.len(), pts) };

        map_send_status(res)
    }

    fn poll_output(&mut self) -> Result<Box<dyn DecodedPicture + '_>, MediaError> {
        let config = self.config.ok_or(MediaError::NotConfigured)?;
        let raw = self.raw.ok_or(MediaError::NotConfigured)?;

        let geom = config.geometry();
        let width = geom.crop_width();
        let height = geom.crop_height();
        let expected_size = (width as usize) * (height as usize) * 4;

        let mut out_bgra = vec![0u8; expected_size];
        let mut out_pts: u64 = 0;

        let res = unsafe {
            fr_ffi_decoder_receive_frame(
                raw.as_ptr(),
                out_bgra.as_mut_ptr(),
                out_bgra.len(),
                &raw mut out_pts,
            )
        };

        map_receive_status(res)?;

        let surface = FfiSurface::new(
            self.expected_backend,
            PixelFormat::Bgra8,
            width,
            height,
            out_bgra,
        );

        Ok(Box::new(FfiDecodedPicture::new(out_pts, surface)))
    }

    fn configuration(&self) -> Option<CodecConfiguration> {
        self.config
    }
}

impl Drop for FfmpegDecoder {
    fn drop(&mut self) {
        if let Some(raw) = self.raw.take() {
            unsafe {
                fr_ffi_decoder_free(raw.as_ptr());
            }
        }
    }
}
