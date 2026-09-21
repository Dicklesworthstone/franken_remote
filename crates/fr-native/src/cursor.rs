//! Separate X11 cursor observation on the ORIGINAL capture connection.
//! No framebuffer readback, HEVC submission, input authority, or source-age proof.
//! Xlib/driver calls belong in a supervised media process, not an authority task.
use crate::NativeError;
use core::{
    ffi::{c_int, c_ulong, c_void},
    ptr::NonNull,
};
use fr_core::limits::ProtocolLimits;
use fr_wire::cursor::{CursorShape, MAX_CURSOR_DIMENSION};

// XFixesCursorImage's public ABI prefix. Newer library versions append an Atom
// and a name; neither is read, copied, logged, nor used for cursor identity.
#[repr(C)]
struct Image {
    x: i16,
    y: i16,
    width: u16,
    height: u16,
    xhot: u16,
    yhot: u16,
    serial: c_ulong,
    pixels: *const c_ulong,
}
#[link(name = "libXfixes.so.3", kind = "dylib", modifiers = "+verbatim")]
unsafe extern "C" {
    fn XFixesQueryExtension(d: *mut c_void, event: *mut c_int, error: *mut c_int) -> c_int;
    fn XFixesQueryVersion(d: *mut c_void, major: *mut c_int, minor: *mut c_int) -> c_int;
    fn XFixesGetCursorImage(d: *mut c_void) -> *mut Image;
}
#[link(name = "X11")]
unsafe extern "C" {
    fn XFree(data: *mut c_void) -> c_int;
    fn XQueryPointer(
        d: *mut c_void,
        window: c_ulong,
        root: *mut c_ulong,
        child: *mut c_ulong,
        root_x: *mut c_int,
        root_y: *mut c_int,
        window_x: *mut c_int,
        window_y: *mut c_int,
        mask: *mut u32,
    ) -> c_int;
}
struct Allocation(NonNull<Image>);
impl Drop for Allocation {
    fn drop(&mut self) {
        // SAFETY: unmodified allocation returned by XFixesGetCursorImage;
        // no pixel or name pointer outlives this owner, and it is freed once.
        unsafe { XFree(self.0.as_ptr().cast()) };
    }
}

/// A bounded, actually observed shape. RGBA8 uses straight (not premultiplied)
/// alpha. Coordinates are relative to the selected capture rectangle. An outside
/// pointer is represented by `None`, without fetching its potentially sensitive
/// shape. This is confirmed OS cursor state, NOT a view-freshness witness.
pub struct CursorSnapshot {
    serial: u32,
    x: i32,
    y: i32,
    width: u16,
    height: u16,
    hotspot_x: u16,
    hotspot_y: u16,
    rgba: Vec<u8>,
}
impl core::fmt::Debug for CursorSnapshot {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CursorSnapshot")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("bytes", &self.rgba.len())
            .finish_non_exhaustive()
    }
}
impl CursorSnapshot {
    /// A borrowed private-pipe observation. It deliberately has no visibility
    /// field, so a successful OS query cannot be mistaken for a visible cursor.
    pub fn observation(&self) -> fr_media::worker::cursor::Snapshot<'_> {
        fr_media::worker::cursor::Snapshot {
            native_serial: self.serial,
            x: self.x,
            y: self.y,
            width: self.width,
            height: self.height,
            hotspot_x: self.hotspot_x,
            hotspot_y: self.hotspot_y,
            rgba: &self.rgba,
        }
    }
    /// Native shape token, scoped to this X server lifetime. Network senders
    /// must assign their own non-reused, connection-scoped shape identifiers.
    pub const fn native_serial(&self) -> u32 {
        self.serial
    }
    pub const fn position(&self) -> (i32, i32) {
        (self.x, self.y)
    }
    /// Neutral shape metadata: visibility stays unset. XFIXES exposes the
    /// logical cursor image even when another client hides the server cursor;
    /// it cannot certify actual visibility or pointer-lock state. Only a
    /// separately qualified owner may set those wire flags.
    pub fn shape(&self, shape_id: u32) -> CursorShape<'_> {
        CursorShape {
            shape_id,
            width: self.width,
            height: self.height,
            hotspot_x: self.hotspot_x,
            hotspot_y: self.hotspot_y,
            scale_1000: 1000,
            flags: 0,
            rgba: &self.rgba,
        }
    }
}

/// Capture-rectangle scope, expressed in the original X root coordinate space.
#[derive(Clone, Copy)]
pub(crate) struct Area {
    pub(crate) x: i32,
    pub(crate) y: i32,
    pub(crate) width: u32,
    pub(crate) height: u32,
}
impl Area {
    fn contains(self, x: i32, y: i32) -> bool {
        let x = i64::from(x) - i64::from(self.x);
        let y = i64::from(y) - i64::from(self.y);
        x >= 0 && y >= 0 && x < i64::from(self.width) && y < i64::from(self.height)
    }
}
// SAFETY CONTRACT: display and root belong to the caller's live, thread-confined
// capture owner. All pointers remain borrowed only through this synchronous call.
unsafe fn position(display: NonNull<c_void>, root: c_ulong) -> Option<(i32, i32)> {
    let (mut actual, mut child) = (0, 0);
    let (mut rx, mut ry, mut wx, mut wy, mut mask) = (0, 0, 0, 0, 0);
    // SAFETY: all scalar outputs are initialized and writable; caller owns Xlib.
    let same_screen = unsafe {
        XQueryPointer(
            display.as_ptr(),
            root,
            &raw mut actual,
            &raw mut child,
            &raw mut rx,
            &raw mut ry,
            &raw mut wx,
            &raw mut wy,
            &raw mut mask,
        )
    };
    (same_screen != 0 && actual == root).then_some((rx, ry))
}

/// Borrow only an existing capture connection; never reopen DISPLAY here.
/// A cursor moving between the image and position queries yields typed `NeedInput`,
/// not coordinates or pixels asserted to belong to another selected display.
pub(crate) unsafe fn snapshot(
    display: NonNull<c_void>,
    root: c_ulong,
    area: Area,
    limits: &ProtocolLimits,
) -> Result<Option<CursorSnapshot>, NativeError> {
    let (mut event, mut error, mut major, mut minor) = (0, 0, 1, 0);
    // SAFETY: live original connection and scalar outputs, synchronously borrowed.
    if unsafe { XFixesQueryExtension(display.as_ptr(), &raw mut event, &raw mut error) } == 0
        || unsafe { XFixesQueryVersion(display.as_ptr(), &raw mut major, &raw mut minor) } == 0
        || major < 1
    {
        return Err(NativeError::Unavailable);
    }
    // Check selected scope BEFORE retrieving a shape from the OS.
    // SAFETY: same connection/root contract as this function.
    let before = unsafe { position(display, root) };
    if !before.is_some_and(|(x, y)| area.contains(x, y)) {
        return Ok(None);
    }
    // SAFETY: XFixes returns one library-owned image plus its native pixel array.
    // The X server/library are a native trust boundary: their internal allocation
    // occurs before our checks, but no unchecked size reaches a Rust allocation.
    let allocation = Allocation(
        NonNull::new(unsafe { XFixesGetCursorImage(display.as_ptr()) })
            .ok_or(NativeError::Allocation)?,
    );
    // SAFETY: the non-null allocation contains the documented ABI prefix.
    let image = unsafe { allocation.0.as_ref() };
    // Check screen and position again, before copying/returning any shape bytes.
    // SAFETY: display is still borrowed from the same original capture owner.
    let after = unsafe { position(display, root) };
    if !after.is_some_and(|(x, y)| area.contains(x, y)) {
        return Ok(None);
    }
    let (x, y) = (i32::from(image.x), i32::from(image.y));
    if after != Some((x, y)) {
        return Err(NativeError::NeedInput);
    }
    let pixels = usize::from(image.width) * usize::from(image.height);
    let bytes = pixels
        .checked_mul(4)
        .ok_or(NativeError::InvalidConfiguration)?;
    let total = bytes
        .checked_add(fr_media::worker::HEADER_BYTES + fr_media::worker::cursor::PREFIX_BYTES)
        .ok_or(NativeError::InvalidConfiguration)?;
    if image.width == 0
        || image.height == 0
        || image.width > MAX_CURSOR_DIMENSION
        || image.height > MAX_CURSOR_DIMENSION
        || image.xhot >= image.width
        || image.yhot >= image.height
        || image.pixels.is_null()
        || total > limits.max_control_message_bytes() as usize
    {
        return Err(NativeError::InvalidConfiguration);
    }
    let mut rgba = Vec::new();
    rgba.try_reserve_exact(bytes)
        .map_err(|_| NativeError::Allocation)?;
    // SAFETY: the native API provides exactly width*height unsigned-long pixels;
    // geometry/bytes have been bounded before the slice and destination allocation.
    for &pixel in unsafe { core::slice::from_raw_parts(image.pixels, pixels) } {
        let a = ((pixel >> 24) & 255) as u32;
        for shift in [16, 8, 0] {
            let c = ((pixel >> shift) & 255) as u32;
            let straight = (c * 255 + a / 2).checked_div(a).unwrap_or(0).min(255);
            rgba.push(u8::try_from(straight).expect("bounded channel"));
        }
        rgba.push(u8::try_from(a).expect("8-bit alpha"));
    }
    Ok(Some(CursorSnapshot {
        serial: u32::try_from(image.serial).map_err(|_| NativeError::InvalidConfiguration)?,
        x: x.checked_sub(area.x).ok_or(NativeError::GeometryChanged)?,
        y: y.checked_sub(area.y).ok_or(NativeError::GeometryChanged)?,
        width: image.width,
        height: image.height,
        hotspot_x: image.xhot,
        hotspot_y: image.yhot,
        rgba,
    }))
}
