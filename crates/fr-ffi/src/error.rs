//! Error mappings from the C ABI bridge to `fr_media::codec::MediaError`.

use core::ffi::c_int;
use fr_media::codec::MediaError;

pub const FR_FFI_OK: c_int = 0;
pub const FR_FFI_AGAIN: c_int = 1;
pub const FR_FFI_EOF: c_int = 2;
pub const FR_FFI_INVALID: c_int = -1;
pub const FR_FFI_UNAVAILABLE: c_int = -2;
pub const FR_FFI_MEMORY: c_int = -3;
pub const FR_FFI_CODEC: c_int = -4;
pub const FR_FFI_DEVICE_LOST: c_int = -5;
pub const FR_FFI_FATAL: c_int = -6;

/// Maps a C return code for a submit/send operation.
#[inline]
pub fn map_send_status(status: c_int) -> Result<(), MediaError> {
    match status {
        FR_FFI_OK => Ok(()),
        FR_FFI_AGAIN => Err(MediaError::Backpressure),
        FR_FFI_EOF => Err(MediaError::EndOfStream),
        FR_FFI_DEVICE_LOST => Err(MediaError::DeviceLost),
        FR_FFI_INVALID => Err(MediaError::ConfigMismatch),
        _ => Err(MediaError::Fatal),
    }
}

/// Maps a C return code for a poll/receive operation.
#[inline]
pub fn map_receive_status(status: c_int) -> Result<(), MediaError> {
    match status {
        FR_FFI_OK => Ok(()),
        FR_FFI_AGAIN => Err(MediaError::NeedMoreInput),
        FR_FFI_EOF => Err(MediaError::EndOfStream),
        FR_FFI_DEVICE_LOST => Err(MediaError::DeviceLost),
        _ => Err(MediaError::Fatal),
    }
}
