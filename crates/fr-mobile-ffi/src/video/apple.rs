//! The `VideoToolbox` backend: `src/vt_decode.c`, linked by `build.rs`.
use super::Error;
use core::ffi::{c_int, c_void};
use core::ptr::NonNull;

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
    /// Releases one +1 picture reference (tests release what the slot hands out).
    pub(super) fn fr_vt_image_release(image: *mut c_void);
    fn fr_vt_image_describe(
        image: *mut c_void,
        width: *mut u32,
        height: *mut u32,
        format: *mut u32,
        io_surface: *mut c_int,
    ) -> c_int;
    fn fr_vt_image_mean_luma(image: *mut c_void, mean: *mut f64) -> c_int;
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

/// One `VideoToolbox` decompression session.
pub(super) struct RawDecoder(NonNull<c_void>);
// SAFETY: the VideoToolbox session is used by one thread at a time (`&mut
// self`); its output callback runs synchronously inside decode.
unsafe impl Send for RawDecoder {}
impl RawDecoder {
    pub(super) fn new(
        [vps, sps, pps]: [&[u8]; 3],
        width: u32,
        height: u32,
        full_range: bool,
    ) -> Result<Self, Error> {
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
                width,
                height,
                c_int::from(full_range),
                &raw mut raw,
            )
        })?;
        NonNull::new(raw).map(Self).ok_or(Error::Allocation)
    }
    pub(super) fn hardware(&self) -> Option<bool> {
        // SAFETY: live decoder handle owned by this thread.
        match unsafe { fr_vt_decoder_hardware(self.0.as_ptr()) } {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        }
    }
    /// Decode one admitted sample: length-prefixed NAL units without
    /// in-band parameter sets (`HevcGuard::prepare_hvc1`).
    pub(super) fn decode(
        &mut self,
        sample: &[u8],
        _idr: bool,
        pts: i64,
        present: bool,
    ) -> Result<Option<Image>, Error> {
        let mut image = core::ptr::null_mut();
        // SAFETY: the admitted sample lives for this synchronous call and the
        // glue copies it into a CoreMedia block buffer before decoding; the
        // out pointer receives a +1 picture only when requested.
        status(unsafe {
            fr_vt_decoder_decode(
                self.0.as_ptr(),
                sample.as_ptr(),
                sample.len(),
                pts,
                if present {
                    &raw mut image
                } else {
                    core::ptr::null_mut()
                },
            )
        })?;
        Ok(NonNull::new(image).map(Image))
    }
}
impl Drop for RawDecoder {
    fn drop(&mut self) {
        // SAFETY: unique owner; invalidates the session before releasing it.
        unsafe { fr_vt_decoder_free(self.0.as_ptr()) };
    }
}
