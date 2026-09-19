//! `FFmpeg` hardware HEVC encoder wrapper implementing `fr_media::codec::Encoder`.

use core::ffi::c_int;
use core::marker::PhantomData;
use core::ptr::NonNull;

use fr_core::ids::RecoveryGeneration;
use fr_core::limits::ProtocolLimits;
use fr_media::access_unit::{EncodedAccessUnit, FrameId, FrameKind};
use fr_media::codec::{EncodeRequest, Encoder, MediaError};
use fr_media::config::CodecConfiguration;
use fr_media::surface::{GpuSurface, SurfaceBackend};

use crate::error::{map_receive_status, map_send_status};

#[repr(C)]
struct FrFfiEncoder {
    _opaque: [u8; 0],
}

unsafe extern "C" {
    fn fr_ffi_encoder_new(
        backend: c_int,
        width: c_int,
        height: c_int,
        fps: c_int,
        bitrate: c_int,
        max_gop: c_int,
        out: *mut *mut FrFfiEncoder,
    ) -> c_int;

    fn fr_ffi_encoder_send_frame(
        enc: *mut FrFfiEncoder,
        bgra_data: *const u8,
        bgra_len: usize,
        pts: u64,
        force_idr: c_int,
    ) -> c_int;

    fn fr_ffi_encoder_receive_packet(
        enc: *mut FrFfiEncoder,
        out_buf: *mut u8,
        out_capacity: usize,
        out_len: *mut usize,
        out_pts: *mut u64,
        out_is_idr: *mut c_int,
    ) -> c_int;

    fn fr_ffi_encoder_drain(enc: *mut FrFfiEncoder) -> c_int;

    fn fr_ffi_encoder_free(enc: *mut FrFfiEncoder);

    fn fr_ffi_simulate_device_loss(enc: *mut FrFfiEncoder);
}

/// Hardware/software encoder backend selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FfiEncoderBackend {
    Nvenc = 0,
    Vaapi = 1,
    Amf = 2,
    Qsv = 3,
    SoftwareExplicit = 4,
    MockSimulated = -1,
}

/// FFmpeg-backed HEVC encoder implementing `fr_media::codec::Encoder`.
///
/// Thread-confined: non-`Sync`, strictly isolated from the broker authority path.
pub struct FfmpegEncoder {
    raw: Option<NonNull<FrFfiEncoder>>,
    backend: FfiEncoderBackend,
    config: Option<CodecConfiguration>,
    limits: ProtocolLimits,
    expected_backend: SurfaceBackend,
    fps: u32,
    bitrate: u32,
    frame_counter: u64,
    last_frame_id: Option<FrameId>,
    recovery_gen: RecoveryGeneration,
    idr_required: bool,
    _marker: PhantomData<*mut ()>,
}

// SAFETY: Send is safe because all resources are unique and transferred across threads cleanly.
// Sync is forbidden because FFmpeg contexts are not thread-safe.
unsafe impl Send for FfmpegEncoder {}

impl FfmpegEncoder {
    /// Creates a new unconfigured encoder instance with explicit backend and limits.
    pub fn new(
        backend: FfiEncoderBackend,
        expected_backend: SurfaceBackend,
        fps: u32,
        bitrate: u32,
    ) -> Self {
        Self {
            raw: None,
            backend,
            config: None,
            limits: ProtocolLimits::ABSOLUTE,
            expected_backend,
            fps,
            bitrate,
            frame_counter: 0,
            last_frame_id: None,
            recovery_gen: RecoveryGeneration::INITIAL,
            idr_required: true,
            _marker: PhantomData,
        }
    }

    /// Simulates hardware device loss for testing and fault recovery validation.
    pub fn simulate_device_loss(&mut self) {
        if let Some(raw) = self.raw {
            unsafe {
                fr_ffi_simulate_device_loss(raw.as_ptr());
            }
        }
    }

    /// Flushes and drains any remaining packets in the encoder.
    pub fn drain(&mut self) -> Result<(), MediaError> {
        let raw = self.raw.ok_or(MediaError::NotConfigured)?;
        let res = unsafe { fr_ffi_encoder_drain(raw.as_ptr()) };
        map_send_status(res)
    }
}

impl Encoder for FfmpegEncoder {
    fn configure(&mut self, config: CodecConfiguration) -> Result<(), MediaError> {
        // Clean up previous instance if already open
        if let Some(raw) = self.raw.take() {
            unsafe {
                fr_ffi_encoder_free(raw.as_ptr());
            }
        }

        let geom = config.geometry();
        let width = geom.crop_width().cast_signed();
        let height = geom.crop_height().cast_signed();
        let fps = self.fps.cast_signed();
        let bitrate = self.bitrate.cast_signed();
        let max_gop = config.gop().max_gop_frames().cast_signed();

        let mut out_ptr: *mut FrFfiEncoder = core::ptr::null_mut();
        let res = unsafe {
            fr_ffi_encoder_new(
                self.backend as c_int,
                width,
                height,
                fps,
                bitrate,
                max_gop,
                &raw mut out_ptr,
            )
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
        self.idr_required = true;
        self.last_frame_id = None;
        Ok(())
    }

    fn submit(
        &mut self,
        surface: &dyn GpuSurface,
        request: EncodeRequest,
    ) -> Result<(), MediaError> {
        let config = self.config.ok_or(MediaError::NotConfigured)?;
        let raw = self.raw.ok_or(MediaError::NotConfigured)?;

        if surface.backend() != self.expected_backend
            && self.backend != FfiEncoderBackend::MockSimulated
        {
            return Err(MediaError::WrongBackend {
                expected: self.expected_backend,
                found: surface.backend(),
            });
        }

        let geom = config.geometry();
        if surface.width() != geom.crop_width() || surface.height() != geom.crop_height() {
            return Err(MediaError::ConfigMismatch);
        }

        let raw_pixels = surface.raw_bytes().ok_or(MediaError::Fatal)?;
        let expected_len = (surface.width() as usize) * (surface.height() as usize) * 4;
        if raw_pixels.len() < expected_len {
            return Err(MediaError::Fatal);
        }

        self.frame_counter += 1;
        let pts = self.frame_counter;
        let force_idr = c_int::from(self.idr_required || request.force_idr);

        let res = unsafe {
            fr_ffi_encoder_send_frame(
                raw.as_ptr(),
                raw_pixels.as_ptr(),
                raw_pixels.len(),
                pts,
                force_idr,
            )
        };

        map_send_status(res)
    }

    fn poll_output(&mut self) -> Result<EncodedAccessUnit, MediaError> {
        let config = self.config.ok_or(MediaError::NotConfigured)?;
        let raw = self.raw.ok_or(MediaError::NotConfigured)?;

        let mut buf = alloc::vec![0u8; 1024 * 1024]; // 1MB output packet capacity
        let mut out_len: usize = 0;
        let mut out_pts: u64 = 0;
        let mut out_is_idr: c_int = 0;

        let res = unsafe {
            fr_ffi_encoder_receive_packet(
                raw.as_ptr(),
                buf.as_mut_ptr(),
                buf.len(),
                &raw mut out_len,
                &raw mut out_pts,
                &raw mut out_is_idr,
            )
        };

        map_receive_status(res)?;

        buf.truncate(out_len);
        let this_frame = FrameId::from_raw(out_pts);
        let is_idr = out_is_idr != 0 || self.idr_required;

        let kind = if is_idr {
            let cur_rec = self.recovery_gen;
            self.recovery_gen = cur_rec.next().unwrap_or(RecoveryGeneration::INITIAL);
            self.idr_required = false;
            FrameKind::Idr { recovery: cur_rec }
        } else {
            let ref_frame = self.last_frame_id.ok_or(MediaError::Fatal)?;
            FrameKind::Predicted {
                references: ref_frame,
            }
        };

        self.last_frame_id = Some(this_frame);

        EncodedAccessUnit::new(
            &self.limits,
            this_frame,
            kind,
            config.generation(),
            1_000_000 / u64::from(self.fps), // duration_us
            buf,
        )
        .map_err(|_| MediaError::Fatal)
    }

    fn configuration(&self) -> Option<CodecConfiguration> {
        self.config
    }
}

impl Drop for FfmpegEncoder {
    fn drop(&mut self) {
        if let Some(raw) = self.raw.take() {
            unsafe {
                fr_ffi_encoder_free(raw.as_ptr());
            }
        }
    }
}
