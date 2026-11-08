//! macOS viewer (fr-5a4w): an `AppKit` window whose `CAMetalLayer` shows
//! `VideoToolbox`-decoded HEVC, behind the narrow Objective-C boundary in
//! `macos_viewer.m`. The Rust HEVC guard admits the hvcC record and every
//! access unit before `VideoToolbox` sees a byte; the glue never parses media.
//! Decoded pictures stay in `IOSurface`-backed buffers that Metal samples
//! directly: no CPU readback, conversion or copy.
//!
//! `Window` lives on the main thread (`AppKit`'s rule). `Surface` and
//! `HevcDecoder` move to one media thread.
use crate::NativeError;
use crate::macos_keys;
use core::ffi::{c_char, c_int, c_void};
use core::marker::PhantomData;
use core::ptr::NonNull;
use fr_core::limits::ProtocolLimits;
use fr_media::access_unit::{EncodedAccessUnit, FrameId, FrameKind};
use fr_media::config::{CodecConfiguration, ColorRange};
use fr_media::hevc::HevcGuard;
use std::time::Duration;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct RawEvent {
    kind: u32,
    code: u32,
    modifiers: u64,
    x: f64,
    y: f64,
    dx: f64,
    dy: f64,
    flags: u32,
    reserved: u32,
}

unsafe extern "C" {
    fn fr_mac_app_init() -> c_int;
    fn fr_mac_window_new(
        width: u32,
        height: u32,
        title: *const c_char,
        title_len: usize,
        out: *mut *mut c_void,
    ) -> c_int;
    fn fr_mac_window_poll(
        window: *mut c_void,
        timeout: f64,
        out: *mut RawEvent,
        capacity: usize,
        overflow: *mut c_int,
    ) -> usize;
    fn fr_mac_window_number(window: *mut c_void) -> isize;
    fn fr_mac_window_renderer(window: *mut c_void) -> *mut c_void;
    fn fr_mac_surface_offscreen(width: u32, height: u32, out: *mut *mut c_void) -> c_int;
    fn fr_mac_window_free(window: *mut c_void);
    fn fr_mac_renderer_presented(renderer: *mut c_void) -> u64;
    fn fr_mac_renderer_release(renderer: *mut c_void);
    fn fr_mac_decoder_new(
        renderer: *mut c_void,
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
    fn fr_mac_decoder_hardware(decoder: *mut c_void) -> c_int;
    fn fr_mac_decoder_decode(decoder: *mut c_void, bytes: *const u8, len: usize, pts: i64)
    -> c_int;
    fn fr_mac_decoder_free(decoder: *mut c_void);
}

fn status(code: c_int) -> Result<(), NativeError> {
    match code {
        0 => Ok(()),
        -1 => Err(NativeError::InvalidConfiguration),
        -2 | -6 => Err(NativeError::Unavailable),
        -4 => Err(NativeError::Allocation),
        -5 => Err(NativeError::GeometryChanged),
        -7 => Err(NativeError::Closed),
        _ => Err(NativeError::Codec),
    }
}

/// Make this process a regular `AppKit` application. Main thread only; call
/// once before opening a window.
pub fn initialize_application() -> Result<(), NativeError> {
    // SAFETY: no arguments; the glue refuses any thread but the main one.
    status(unsafe { fr_mac_app_init() })
}

/// A pointer position relative to the presented picture: (0, 0) is its
/// top-left corner and (1, 1) its bottom-right. Outside the picture (the
/// letterbox bars or beyond the window while dragging) `inside` is false.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PicturePoint {
    pub x: f64,
    pub y: f64,
    pub inside: bool,
}
impl PicturePoint {
    /// The remote pixel under this point, clamped to the picture.
    pub fn pixel(self, width: u32, height: u32) -> (u32, u32) {
        let clamp = |v: f64, n: u32| {
            let max = f64::from(n.saturating_sub(1));
            // Finite, clamped to [0, n - 1] before the conversion.
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let p = (v * f64::from(n)).floor().clamp(0.0, max) as u32;
            p
        };
        let x = if self.x.is_finite() { self.x } else { 0.0 };
        let y = if self.y.is_finite() { self.y } else { 0.0 };
        (clamp(x, width), clamp(y, height))
    }
}

/// One local input or window event, with keys already reduced to HID usages.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum WindowEvent {
    /// A physical key (HID usage page 0x07) changed state.
    Key {
        usage: u16,
        pressed: bool,
        repeat: bool,
    },
    /// A key with no HID keyboard usage (Fn, vendor keys). Never forwarded.
    UnmappedKey {
        keycode: u16,
    },
    Pointer(PicturePoint),
    /// `AppKit` button number: 0 left, 1 right, 2 middle, then extra buttons.
    Button {
        button: u8,
        pressed: bool,
        at: PicturePoint,
    },
    /// `AppKit` scrolling deltas: points when `precise` (trackpads), else lines.
    Scroll {
        dx: f64,
        dy: f64,
        precise: bool,
        at: PicturePoint,
    },
    /// The user asked to close the window. It stays open until dropped.
    CloseRequested,
    /// Keyboard focus left the window: held remote keys must be released.
    FocusLost,
    FocusGained,
    /// Events arrived faster than they were polled and some were lost.
    Overflow,
}

/// The viewer window. Main thread only.
pub struct Window {
    raw: NonNull<c_void>,
    modifiers: Modifiers,
    buffer: Vec<RawEvent>,
    _main_thread: PhantomData<*mut ()>,
}
impl Window {
    /// Opens a window for a `width` x `height` remote picture.
    pub fn open(width: u32, height: u32, title: &str) -> Result<Self, NativeError> {
        let mut raw = core::ptr::null_mut();
        // SAFETY: title bytes live for the call and the glue copies them; the
        // out pointer is writable. The glue refuses a non-main thread.
        status(unsafe {
            fr_mac_window_new(
                width,
                height,
                title.as_ptr().cast(),
                title.len(),
                &raw mut raw,
            )
        })?;
        Ok(Self {
            raw: NonNull::new(raw).ok_or(NativeError::Allocation)?,
            modifiers: Modifiers::default(),
            buffer: vec![RawEvent::default(); 256],
            _main_thread: PhantomData,
        })
    }
    /// The `CGWindowID` (for `screencapture -l`).
    pub fn number(&self) -> isize {
        // SAFETY: live window handle on its owning thread.
        unsafe { fr_mac_window_number(self.raw.as_ptr()) }
    }
    /// A presentation handle for the media thread.
    pub fn surface(&self) -> Surface {
        // SAFETY: live window handle; the glue returns a +1 renderer reference.
        let raw = unsafe { fr_mac_window_renderer(self.raw.as_ptr()) };
        Surface {
            raw: NonNull::new(raw).expect("a live window always has a renderer"),
        }
    }
    /// Runs the `AppKit` event loop for up to `timeout` (returning at the first
    /// event) and appends what arrived.
    pub fn poll(&mut self, timeout: Duration, events: &mut Vec<WindowEvent>) {
        let mut overflow = 0;
        // SAFETY: the buffer is writable for its length; live handle, main thread.
        let n = unsafe {
            fr_mac_window_poll(
                self.raw.as_ptr(),
                timeout.as_secs_f64(),
                self.buffer.as_mut_ptr(),
                self.buffer.len(),
                &raw mut overflow,
            )
        };
        for i in 0..n.min(self.buffer.len()) {
            let raw = self.buffer[i];
            self.translate(raw, events);
        }
        if overflow != 0 {
            events.push(WindowEvent::Overflow);
        }
    }
    fn translate(&mut self, e: RawEvent, events: &mut Vec<WindowEvent>) {
        let at = PicturePoint {
            x: e.x,
            y: e.y,
            inside: e.flags & 1 != 0,
        };
        let keycode = u16::try_from(e.code).unwrap_or(u16::MAX);
        match e.kind {
            1 | 2 => events.push(match macos_keys::usage(keycode) {
                Some(usage) => WindowEvent::Key {
                    usage,
                    pressed: e.kind == 1,
                    repeat: e.kind == 1 && e.flags & 1 != 0,
                },
                None => WindowEvent::UnmappedKey { keycode },
            }),
            3 => self.modifiers.change(keycode, e.modifiers, events),
            4 => events.push(WindowEvent::Pointer(at)),
            5 | 6 => {
                if let Ok(button) = u8::try_from(e.code) {
                    events.push(WindowEvent::Button {
                        button,
                        pressed: e.kind == 5,
                        at,
                    });
                }
            }
            7 => events.push(WindowEvent::Scroll {
                dx: e.dx,
                dy: e.dy,
                precise: e.flags & 2 != 0,
                at,
            }),
            8 => events.push(WindowEvent::CloseRequested),
            9 => {
                // Released remotely by the session; local tracking starts over.
                self.modifiers = Modifiers::default();
                events.push(WindowEvent::FocusLost);
            }
            10 => events.push(WindowEvent::FocusGained),
            _ => {}
        }
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        // SAFETY: unique owner of the +1 window reference, on the main thread.
        unsafe { fr_mac_window_free(self.raw.as_ptr()) };
    }
}

/// Held modifier keys, for turning `FlagsChanged` into presses and releases.
#[derive(Clone, Copy, Debug, Default)]
struct Modifiers {
    held: u8,
}
impl Modifiers {
    /// `FlagsChanged` names the key but not the direction: read it from the
    /// side-specific device bit, falling back to the shared flag for
    /// keyboards that report no device bits. Duplicates are dropped.
    fn change(&mut self, keycode: u16, flags: u64, events: &mut Vec<WindowEvent>) {
        if keycode == macos_keys::CAPS_LOCK {
            // Each change is one press of a toggling key.
            for pressed in [true, false] {
                events.push(WindowEvent::Key {
                    usage: 0x39,
                    pressed,
                    repeat: false,
                });
            }
            return;
        }
        let Some(m) = macos_keys::modifier(keycode) else {
            events.push(WindowEvent::UnmappedKey { keycode });
            return;
        };
        let pressed = if flags & macos_keys::DEVICE_MASKS != 0 || flags & m.flag == 0 {
            flags & m.device_mask != 0
        } else {
            flags & m.flag != 0
        };
        let bit = 1_u8 << (m.usage - 0xe0);
        if pressed == (self.held & bit != 0) {
            return;
        }
        self.held ^= bit;
        events.push(WindowEvent::Key {
            usage: m.usage,
            pressed,
            repeat: false,
        });
    }
}

/// The window's Metal presenter, shared with the media thread.
pub struct Surface {
    raw: NonNull<c_void>,
}
// SAFETY: the renderer serializes every access behind its own lock and holds
// no thread-affine AppKit state; CAMetalLayer drawables are thread-safe.
unsafe impl Send for Surface {}
impl Surface {
    /// A presenter with no window, drawing into a detached layer of
    /// `width` x `height` pixels. Any thread: decode tests and benchmarks.
    pub fn offscreen(width: u32, height: u32) -> Result<Self, NativeError> {
        let mut raw = core::ptr::null_mut();
        // SAFETY: scalar sizes and a writable out pointer.
        status(unsafe { fr_mac_surface_offscreen(width, height, &raw mut raw) })?;
        Ok(Self {
            raw: NonNull::new(raw).ok_or(NativeError::Allocation)?,
        })
    }
    /// Pictures submitted to the display (presentation requests, not a
    /// visibility witness).
    pub fn presented(&self) -> u64 {
        // SAFETY: live renderer reference.
        unsafe { fr_mac_renderer_presented(self.raw.as_ptr()) }
    }
}
impl Drop for Surface {
    fn drop(&mut self) {
        // SAFETY: releases exactly the +1 reference this owner holds.
        unsafe { fr_mac_renderer_release(self.raw.as_ptr()) };
    }
}

/// The VPS, SPS and PPS of a canonical record (`HevcGuard::decoder_record`
/// layout: 23-byte header, then three single-NAL arrays). Call only on a
/// record the guard admitted.
fn parameter_sets(record: &[u8]) -> Option<[&[u8]; 3]> {
    let mut offset: usize = 23;
    let mut sets: [&[u8]; 3] = [&[]; 3];
    for (slot, kind) in sets.iter_mut().zip(32_u8..=34) {
        let header = record.get(offset..offset.checked_add(5)?)?;
        if header[..3] != [0x80 | kind, 0, 1] {
            return None;
        }
        let length = usize::from(u16::from_be_bytes([header[3], header[4]]));
        offset += 5;
        *slot = record.get(offset..offset.checked_add(length)?)?;
        offset += length;
    }
    (offset == record.len()).then_some(sets)
}

/// `VideoToolbox` HEVC decode presenting straight to a `Surface`.
pub struct HevcDecoder {
    raw: NonNull<c_void>,
    surface: Surface,
    config: CodecConfiguration,
    limits: ProtocolLimits,
    admission: HevcGuard,
    last: Option<FrameId>,
    closed: bool,
}
// SAFETY: the VideoToolbox session is used by one thread at a time (`&mut
// self`); its output callback runs synchronously inside decode.
unsafe impl Send for HevcDecoder {}
impl HevcDecoder {
    /// Admits `record` with the HEVC guard, then configures `VideoToolbox` from
    /// its parameter sets. A refused record never reaches `VideoToolbox`.
    pub fn new(
        surface: Surface,
        config: CodecConfiguration,
        limits: ProtocolLimits,
        record: &[u8],
    ) -> Result<Self, NativeError> {
        let admission = HevcGuard::from_decoder_record(config, limits, 4, record)
            .map_err(|_| NativeError::InvalidConfiguration)?;
        let [vps, sps, pps] = parameter_sets(record).ok_or(NativeError::InvalidConfiguration)?;
        let g = config.geometry();
        let full_range = c_int::from(matches!(config.color().range, ColorRange::Full));
        let mut raw = core::ptr::null_mut();
        // SAFETY: admitted parameter-set slices live for the call and
        // CoreMedia copies them; renderer and out pointers are valid.
        status(unsafe {
            fr_mac_decoder_new(
                surface.raw.as_ptr(),
                vps.as_ptr(),
                vps.len(),
                sps.as_ptr(),
                sps.len(),
                pps.as_ptr(),
                pps.len(),
                g.crop_width(),
                g.crop_height(),
                full_range,
                &raw mut raw,
            )
        })?;
        Ok(Self {
            raw: NonNull::new(raw).ok_or(NativeError::Allocation)?,
            surface,
            config,
            limits,
            admission,
            last: None,
            closed: false,
        })
    }
    /// Whether `VideoToolbox` chose a hardware decoder, when it says.
    pub fn hardware(&self) -> Option<bool> {
        // SAFETY: live decoder handle owned by this thread.
        match unsafe { fr_mac_decoder_hardware(self.raw.as_ptr()) } {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        }
    }
    pub fn surface(&self) -> &Surface {
        &self.surface
    }
    /// Admits, decodes and presents one access unit. Admission and decode are
    /// one transaction: a refused or failed unit leaves the guard unchanged.
    /// A decode failure closes the decoder; recovery is a new IDR on a new one.
    pub fn submit(&mut self, unit: &EncodedAccessUnit) -> Result<(), NativeError> {
        if self.closed {
            return Err(NativeError::Closed);
        }
        if unit.config_generation() != self.config.generation()
            || unit.frame().as_raw() > i64::MAX as u64
            || self.last.is_some_and(|n| unit.frame() <= n)
        {
            return Err(NativeError::StaleGeneration);
        }
        self.limits
            .validate_access_unit_len(unit.bytes().len())
            .map_err(|_| NativeError::UnsupportedBitstream)?;
        let idr = unit.is_idr();
        if !idr
            && unit.kind()
                != (FrameKind::Predicted {
                    references: self.last.ok_or(NativeError::UnsupportedBitstream)?,
                })
        {
            return Err(NativeError::UnsupportedBitstream);
        }
        let mut admission = self.admission.clone();
        // Checks every NAL against the frozen parameter sets, then strips the
        // in-band copies: VideoToolbox decodes against the configured format.
        let sample = admission
            .prepare_hvc1(unit.bytes(), idr)
            .map_err(|_| NativeError::UnsupportedBitstream)?;
        let pts = i64::try_from(unit.capture_micros()).unwrap_or(i64::MAX);
        let bytes = sample.bytes();
        // SAFETY: the admitted sample lives for this synchronous call and the
        // glue copies it into a CoreMedia block buffer before decoding.
        if let Err(error) = status(unsafe {
            fr_mac_decoder_decode(self.raw.as_ptr(), bytes.as_ptr(), bytes.len(), pts)
        }) {
            self.closed = true;
            return Err(error);
        }
        self.admission = admission;
        self.last = Some(unit.frame());
        Ok(())
    }
}
impl Drop for HevcDecoder {
    fn drop(&mut self) {
        // SAFETY: unique owner; invalidates the session before releasing it.
        unsafe { fr_mac_decoder_free(self.raw.as_ptr()) };
    }
}

#[cfg(test)]
#[path = "macos/tests.rs"]
mod tests;
