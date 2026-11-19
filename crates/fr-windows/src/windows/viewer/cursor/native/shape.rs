//! UI-thread ownership of a nonshared Win32 cursor. Microsoft documents this
//! top-down BGRA/AND-mask path under "Creating an Alpha Blended Cursor".
//! `CreateIconIndirect` copies both bitmaps; they are never selected into a DC.
use crate::cursor_image::Image;
use std::{ffi::c_void, mem::size_of, ptr};
use windows::Win32::{
    Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateDIBSection, DIB_RGB_COLORS, DeleteObject,
        HBITMAP, HGDIOBJ, RGBQUAD,
    },
    UI::WindowsAndMessaging::{
        CreateIconIndirect, DestroyCursor, GetCursor, HCURSOR, ICONINFO, SetCursor,
    },
};

#[repr(C)]
struct BitmapInfo {
    header: BITMAPINFOHEADER,
    /// The monochrome AND mask needs both entries, unlike BITMAPINFO's
    /// flexible-array placeholder. The 32-bit color DIB uses neither.
    colors: [RGBQUAD; 2],
}

struct Bitmap(HBITMAP);

impl Bitmap {
    fn new(width: u16, height: u16, depth: u16, bytes: &[u8]) -> Result<Self, ()> {
        let stride = match depth {
            1 => usize::from(width).div_ceil(32) * 4,
            32 => usize::from(width) * 4,
            _ => return Err(()),
        };
        if width == 0
            || height == 0
            || width > 256
            || height > 256
            || stride.checked_mul(usize::from(height)) != Some(bytes.len())
        {
            return Err(());
        }
        let info = BitmapInfo {
            header: BITMAPINFOHEADER {
                biSize: u32::try_from(size_of::<BITMAPINFOHEADER>()).map_err(|_| ())?,
                biWidth: i32::from(width),
                biHeight: -i32::from(height),
                biPlanes: 1,
                biBitCount: depth,
                biCompression: BI_RGB.0,
                biSizeImage: u32::try_from(bytes.len()).map_err(|_| ())?,
                biClrUsed: if depth == 1 { 2 } else { 0 },
                ..BITMAPINFOHEADER::default()
            },
            colors: [
                RGBQUAD::default(),
                RGBQUAD {
                    rgbBlue: 255,
                    rgbGreen: 255,
                    rgbRed: 255,
                    rgbReserved: 0,
                },
            ],
        };
        let mut data: *mut c_void = ptr::null_mut();
        // SAFETY: repr(C) header followed by the complete two-entry color
        // table. Geometry/stride/length are checked; the output pointer is a
        // writable local. The unique bitmap is owned on this UI thread.
        let bitmap = Self(
            unsafe {
                CreateDIBSection(
                    None,
                    (&raw const info).cast::<BITMAPINFO>(),
                    DIB_RGB_COLORS,
                    &raw mut data,
                    None,
                    0,
                )
            }
            .map_err(|_| ())?,
        );
        if data.is_null() {
            return Err(());
        }
        // SAFETY: successful CreateDIBSection allocated exactly the admitted
        // top-down rows, including DWORD padding. Source and native storage
        // are disjoint and both remain live for this complete bounded copy.
        unsafe { ptr::copy_nonoverlapping(bytes.as_ptr(), data.cast::<u8>(), bytes.len()) };
        Ok(bitmap)
    }

    fn retire(&mut self) -> Result<(), ()> {
        if self.0.is_invalid() {
            return Ok(());
        }
        // SAFETY: this unique bitmap was never selected into a DC; the cursor
        // constructor has completed its synchronous copy before this call.
        if !unsafe { DeleteObject(HGDIOBJ(self.0.0)) }.as_bool() {
            return Err(());
        }
        self.0 = HBITMAP::default();
        Ok(())
    }
}

impl Drop for Bitmap {
    fn drop(&mut self) {
        let _ = self.retire();
    }
}

/// One bounded native image. Never shared, never installed as a class/system
/// cursor, and always removed from this thread's current cursor before destroy.
pub(in super::super) struct Cursor(HCURSOR);

impl Cursor {
    pub(in super::super) fn new(image: &Image) -> Result<Self, ()> {
        if image.shape.bytes() != Some(image.bgra.len()) {
            return Err(());
        }
        let mut color = Bitmap::new(image.shape.width, image.shape.height, 32, &image.bgra)?;
        let mut mask = Bitmap::new(image.shape.width, image.shape.height, 1, &image.mask)?;
        let info = ICONINFO {
            fIcon: false.into(),
            xHotspot: u32::from(image.shape.hotspot_x),
            yHotspot: u32::from(image.shape.hotspot_y),
            hbmMask: mask.0,
            hbmColor: color.0,
        };
        // SAFETY: exact bounded color/mask bitmaps and an in-bounds hotspot.
        // fIcon=false creates a cursor even though the API returns HICON.
        // Windows copies both bitmaps; neither handle escapes this function.
        let icon = unsafe { CreateIconIndirect(&raw const info) }.map_err(|_| ())?;
        let cursor = Self(HCURSOR(icon.0));
        let color_retired = color.retire();
        let mask_retired = mask.retire();
        if color_retired.is_err() || mask_retired.is_err() {
            return Err(());
        }
        Ok(cursor)
    }

    /// Call only for the viewer's own eligible client-area point.
    pub(in super::super) fn assign(&self) -> bool {
        // SAFETY: this UI thread owns the live, nonshared cursor; it remains
        // allocated until another image/default has replaced it.
        unsafe {
            SetCursor(Some(self.0));
            GetCursor() == self.0
        }
    }

    pub(in super::super) fn unselect(&self) -> bool {
        // SAFETY: only remove this exact nonshared image, never another
        // window's cursor. The caller chooses its default/blank policy next.
        unsafe {
            if GetCursor() == self.0 {
                SetCursor(None);
            }
            GetCursor() != self.0
        }
    }

    pub(in super::super) fn retire(&mut self) -> Result<(), ()> {
        if self.0.is_invalid() {
            return Ok(());
        }
        if !self.unselect() {
            return Err(());
        }
        // SAFETY: this unique, nonshared cursor is no longer selected.
        // The caller retains it on failure and admits no replacement work.
        unsafe { DestroyCursor(self.0) }.map_err(|_| ())?;
        self.0 = HCURSOR::default();
        Ok(())
    }
}

impl Drop for Cursor {
    fn drop(&mut self) {
        let _ = self.retire();
    }
}
